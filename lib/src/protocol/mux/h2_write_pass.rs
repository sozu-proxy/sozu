//! Owned state of ONE [`super::h2::ConnectionH2::write_streams`] pass, and the
//! phase machine that pass is driven through.
//!
//! Every counter here used to be a `let` inside that function. They are
//! grouped into one struct because the write path is a drive loop — a `poll`
//! that names the next transmit, a shell that performs it, a `handle` that
//! reports it back — and a value the pre-image kept in a stack slot across
//! the flush loop has to survive that round trip.
//!
//! **This module owns**: the byte counters, flags and scratch vectors of one
//! write pass, their initial values, and [`H2WritePhase`] — the resumption
//! point the next [`super::h2::ConnectionH2::poll_write_target`] call picks
//! up from.
//!
//! **This module does NOT own** the pass's control flow, nor two values one
//! pass touches:
//!
//! - The `Vec<IoSlice<'static>>`. [`super::h2_transmit::gather`] returns
//!   descriptors carrying a `'static` lifetime they do not have, pointing
//!   into `kawa.storage`, and [`super::h2_transmit::confirm`] must discharge
//!   them before the consume that may relocate that storage. Leaving the
//!   vector with the shell keeps that `unsafe` window exactly as wide as the
//!   three adjacent statements that open and close it, and stops the
//!   poll/handle boundary from spanning it.
//! - `ConnectionH2` itself, and everything reachable from it. A phase names
//!   a stream, never borrows one.
//!
//! # The three values a pass may not own from its start
//!
//! The scheduler's loaned `order` buffer, its
//! [`super::h2_scheduler::ReadyIncrementalCensus`], and the
//! [`super::converter::H2ConverterPass`] are exactly the state the inversion
//! must carry across the caller boundary — and exactly the state that cannot
//! exist when the pass begins. Each is excluded for its OWN reason, and they
//! are not interchangeable:
//!
//! - `order` and `census` — **enumeration**. `H2Scheduler::begin_pass`
//!   enumerates `H2StreamTable::streams`, and the `expect_write` resume path
//!   that runs before it can retire a stream from that same map through
//!   `ConnectionH2::remove_dead_stream`. Building them at pass start would
//!   change which streams the pass visits.
//! - `converter` — **buffer stranding**. It enumerates no streams, so the
//!   reason above does not apply. Its constructor takes the three reusable
//!   scratch buffers out of `HpackState` (`take_converter_buf` /
//!   `take_lowercase_buf` / `take_cookie_buf`, each a `mem::take`), and only
//!   `into_buffers` after the per-stream loop hands them back through the
//!   matching `put_*` calls. The resume path's stall ends the pass before
//!   both, so a converter built at pass start would be dropped on every
//!   stalled resume, leaving `HpackState` holding three empty `Vec`s to
//!   re-grow — the reuse the pass exists to provide, discarded on exactly
//!   the backpressure path that repeats most.
//!
//! [`H2WritePhase`] answers that with ownership rather than with an
//! `Option`. The three live together in [`H2ScheduledPass`], which exists
//! **only** inside [`H2WritePhase::Scheduled`]. Before the
//! `Resume → Prepare` transition the phase is
//! [`H2WritePhase::Unscheduled`], which has no room for them; after the tail
//! has taken them it is [`H2WriteStage::Ended`], which has no room either.
//! So no code path anywhere asks whether a converter exists: a step that
//! needs one is reached only by a `match` arm that has already bound it. The
//! inversion adds no `unwrap`, no `expect` and no absent-value state on any
//! of the three.
//!
//! Ownership also settles where the buffers go. [`H2ScheduledPass`] is
//! consumed by value at exactly one site — `ConnectionH2::end_write_pass`,
//! reached only from [`H2ScheduledStep::End`] — so the pass cannot end
//! without the tail having them in hand, and the stalled resume cannot strand
//! them because it never held them.
//!
//! The pass is a local threaded by `&mut`, never a field on `ConnectionH2`:
//! every exit of `write_streams` ends it, so it cannot outlive one call.
//! Resumption ACROSS calls is already expressed by
//! `H2StreamTable::expect_write`.

use std::mem;

use super::{
    GlobalStreamId, StreamId, StreamState, converter::H2ConverterPass, h2::H2WriteTarget,
    h2_scheduler::ReadyIncrementalCensus, parser::H2Error,
};

/// The three values [`H2WritePass`] may not own from its start, owned
/// together once the pass has reached the point where all three are legal.
///
/// Held by [`H2WritePhase::Scheduled`] and consumed by value at the pass
/// tail. There is no constructor here on purpose: `ConnectionH2` builds the
/// three in the one order their dependencies allow, and a `Default` would be
/// an empty scheduler pass this machine has no state for.
pub(super) struct H2ScheduledPass {
    /// Carries the three pooled HPACK scratch buffers and the RFC 7541 §6.3
    /// pending size-update from one stream's `prepare` to the next. Its
    /// buffers return to `HpackState` at the tail.
    pub(super) converter: H2ConverterPass,
    /// The scheduler's loaned RFC 9218 §4 order buffer, walked by
    /// [`H2ScheduledStep::Prepare`]'s index and given back to
    /// `H2Scheduler::end_pass`.
    pub(super) order: Vec<StreamId>,
    /// This pass's same-urgency ready-peer census, also given back to
    /// `H2Scheduler::end_pass`.
    pub(super) census: ReadyIncrementalCensus,
}

/// Where in the pass the core is, and — in [`Self::Scheduled`] — the three
/// values only that half of the pass may hold.
///
/// Two variants, not three, and the split is the one that matters: it is
/// **whether the scheduler pass exists**, which is also the only question
/// `ConnectionH2::handle_write` has to ask to attribute a round. Folding the
/// terminal state into [`Self::Unscheduled`] is what keeps that `match`
/// total with no fallback arm.
pub(super) enum H2WritePhase {
    /// No scheduler pass exists — either the pass has not reached the
    /// `Resume → Prepare` transition yet, or the tail has already taken the
    /// three values back.
    Unscheduled(H2WriteStage),
    /// The scheduler pass is live, and this variant owns it.
    ///
    /// Boxed, and the box is the price of the ownership above: the three
    /// values total 216 bytes against the 48 of [`H2WriteStage`], which
    /// `clippy::large_enum_variant` rejects, and the alternatives are worse.
    /// Consuming them through a `&mut` would need a `Default` on
    /// `ReadyIncrementalCensus` and a `mem::take` variant of
    /// `H2ConverterPass::into_buffers` — two sibling-module additions, to
    /// reach a half-emptied scheduler pass that is an absent value wearing a
    /// different name. One 216-byte allocation per pass that reaches the
    /// scheduler is paid once per WRITABLE event beside at least one
    /// `socket_write_vectored` syscall, and it BUYS a smaller phase: the
    /// 48-byte enum this leaves is moved on every internal step, where the
    /// unboxed 256-byte one would have been.
    Scheduled {
        scheduled: Box<H2ScheduledPass>,
        step: H2ScheduledStep,
    },
}

/// The stages a pass passes through while no scheduler pass exists.
pub(super) enum H2WriteStage {
    /// `H2StreamTable::expect_write` has not been consulted yet.
    Enter,
    /// Draining the parked stream's queue — the pre-image's first
    /// write-pass flush, debug site 2.
    Flush {
        stream_id: StreamId,
        gid: GlobalStreamId,
        /// Read before the flush, as the pre-image reads it, so a stream
        /// retired mid-flush cannot change which state the 1xx check sees.
        stream_state: StreamState,
        /// `Some(amount)` when the SAME stream is also parked waiting for
        /// read space, so each round can re-enable `Ready::READABLE` as soon
        /// as the write frees enough room. This is the pre-image's own
        /// `Option`, a genuine "is this stream parked on both sides"
        /// question, and not an absent value of any kind.
        cross_read_amount: Option<usize>,
    },
    /// The pass has yielded its result. A poll here is the fused answer of a
    /// state machine that is already done; a `Transmit` cannot be yielded
    /// from it, so `handle_write` never attributes a round to it.
    Ended,
}

/// Where in the scheduler loop the core is. The `Prepare` / `Flush` split is
/// LOAD-BEARING: a `Transmit` re-entry that re-ran `Prepare` would issue a
/// second `kawa.prepare`, a second `census.note_fired`, a duplicate
/// `freshly_emitted_rsts` push and a duplicate `DebugEvent::S`, and would
/// fire the flow-control-stall counters for a stream that never stalled.
pub(super) enum H2ScheduledStep {
    /// About to run the eligibility gate, `kawa.prepare`, the window
    /// accounting and `census.note_fired` for `order[index]`.
    Prepare { index: usize },
    /// `Prepare` already ran for this stream; only the flush loop remains.
    /// Re-entered after every `Transmit`, which is the pre-image's
    /// `while !kawa.out.is_empty()` re-expressed as a resumption state.
    Flush(H2WriteCursor),
    /// Past the last stream of `order`, or cut short by a stall: the pass
    /// tail, and the only site that consumes [`H2ScheduledPass`].
    End,
}

/// The stream [`H2ScheduledStep::Flush`] is draining, plus the four facts
/// `Prepare` resolved for it that its post-flush block still needs.
///
/// Carried rather than re-derived: `urgency` and `is_incremental` come from
/// `H2Scheduler::priority`, and re-reading them after the flush would ask
/// the prioriser a second question the pre-image asks once.
#[derive(Clone, Copy)]
pub(super) struct H2WriteCursor {
    /// Index into [`H2ScheduledPass::order`]. Advancing it is the pre-image's
    /// `'outer` iteration step, and it is the ONLY way a stream is visited.
    pub(super) index: usize,
    pub(super) stream_id: StreamId,
    pub(super) gid: GlobalStreamId,
    pub(super) stream_state: StreamState,
    pub(super) urgency: u8,
    pub(super) is_incremental: bool,
}

/// What one step of the phase machine produced.
///
/// [`Self::Advance`] is an internal transition the caller never sees — a
/// stream missing from the map, a queue already empty, the
/// `Resume → Prepare` handover. [`Self::Yield`] is the answer
/// `poll_write_target` returns. Both carry the next phase by value, which is
/// what lets a step take [`H2ScheduledPass`] apart and put it back without
/// any absent-value state in between.
pub(super) enum H2WriteStep {
    Yield(H2WriteTarget, H2WritePhase),
    Advance(H2WritePhase),
}

impl H2WriteStage {
    /// The read amount this stage is paired with, for the round-by-round
    /// `Ready::READABLE` re-enable. `None` outside the resume flush, where
    /// the pre-image passes `None` too.
    pub(super) fn cross_read_amount(&self) -> Option<usize> {
        match self {
            Self::Flush {
                cross_read_amount, ..
            } => *cross_read_amount,
            Self::Enter | Self::Ended => None,
        }
    }
}

/// The `let` bindings of one `write_streams` pass, in the order the pre-image
/// declared them.
pub(super) struct H2WritePass {
    /// Connection-wide `(bytes, streams)` totals used to distribute the
    /// per-session overhead proportionally when a stream is recycled. Computed
    /// once at pass start by `ConnectionH2::compute_stream_byte_totals` and
    /// read by both the resume path and the scheduler loop.
    pub(super) byte_totals: (usize, usize),
    /// `update_readiness_after_write`'s verdict on the last transmit: the
    /// socket could not take more. The ONLY place the write core records a
    /// socket answer, written by `ConnectionH2::handle_write` and read by
    /// the flush states of [`H2WritePhase`] to decide whether to go round
    /// again. `SocketResult` itself never reaches
    /// `ConnectionH2::poll_write_target`.
    ///
    /// It is NOT `status != Continue`: `update_readiness` calls a pass
    /// stalled **iff `size == 0`**, so a write that moved bytes and reported
    /// `WouldBlock` must go round again or the response is truncated.
    pub(super) stalled: bool,
    /// Bytes the `expect_write` resume path moved. Deliberately SEPARATE from
    /// [`Self::total_bytes_written`]: it never reaches `finalize_write`, so a
    /// resume-only pass does not read as pass progress and
    /// `h2_close::finalize_action` quiesces instead of retaining
    /// `Ready::WRITABLE`. Fusing the two is a behaviour change, pinned by
    /// `resume_path_bytes_do_not_make_the_pass_progress`.
    pub(super) resume_bytes: usize,
    /// Streams that finished their lifecycle during this pass, retired after
    /// the loop rather than inside it: `try_recycle_server_stream` reads
    /// `streams().len() == 1` to decide whether one stream inherits the whole
    /// connection overhead pool, and retiring inline would let a later
    /// completer of the same pass see that count while other streams are still
    /// live.
    pub(super) completed_streams: Vec<(StreamId, GlobalStreamId, Option<mio::Token>, bool)>,
    /// Whether any scheduler-loop flush reached the socket at all. Set by the
    /// MAIN LOOP only — the resume path passes `None` for this flag — and read
    /// by `finalize_write` to decide whether it owes its own flush.
    pub(super) socket_write: bool,
    /// Total outbound bytes emitted across all stream flushes this pass —
    /// `finalize_write` uses this to distinguish a voluntary scheduler
    /// yield (progress + pending back-buffer, LIFECYCLE §9 invariant 16)
    /// from a no-progress wait state (e.g. flow-control starvation).
    ///
    /// Accumulated with a saturating add, never overwritten: a later stream
    /// that writes nothing must not erase an earlier stream's progress, which
    /// `a_later_zero_byte_stream_does_not_erase_the_pass_progress` pins.
    pub(super) total_bytes_written: usize,
    /// Collect every fresh RST_STREAM emitted via the converter
    /// (`initialize` chokepoint or the HPACK over-budget abort path)
    /// so we can run `account_emitted_rst` for each one AFTER the loop.
    /// This is an ORDERING requirement, not a borrow workaround: a
    /// MadeYouReset cap trip makes `account_emitted_rst` return a GOAWAY
    /// result that ends the pass, and every stream in the pass's
    /// `order` must get its write before that preemption.
    pub(super) freshly_emitted_rsts: Vec<H2Error>,
    /// Flow-control bytes the current stream's `kawa.prepare` consumed.
    /// Hoisted out of the eligibility gate so the post-flush
    /// flow-control-stall classification can see how many flow-control bytes
    /// this pass moved. Reset at the top of every scheduler-loop iteration.
    pub(super) consumed: i32,
    /// Bytes the current stream's flush moved. Reset at the top of every
    /// scheduler-loop iteration and folded into [`Self::total_bytes_written`]
    /// once that stream's flush has ended.
    pub(super) stream_bytes: usize,
    /// Where the pass is, and — once the scheduler pass exists — the three
    /// values that half of the pass owns. Public to `mux` because
    /// `ConnectionH2::poll_write_target` takes it apart and puts it back on
    /// every call, and `ConnectionH2::handle_write` reads it to attribute
    /// the round it is reporting.
    pub(super) phase: H2WritePhase,
}

impl H2WritePass {
    /// Begin a pass over `byte_totals`, the connection-wide
    /// `(bytes, streams)` pair the recycle path distributes overhead against.
    ///
    /// Every other field starts empty or zero, exactly as its pre-image `let`
    /// did. Hoisting those initialisations to pass start is inert: an empty
    /// `Vec` allocates nothing, and nothing reads any of them before the
    /// statement that used to declare them.
    pub(super) fn new(byte_totals: (usize, usize)) -> Self {
        Self {
            byte_totals,
            stalled: false,
            resume_bytes: 0,
            completed_streams: Vec::new(),
            socket_write: false,
            total_bytes_written: 0,
            freshly_emitted_rsts: Vec::new(),
            consumed: 0,
            stream_bytes: 0,
            phase: H2WritePhase::Unscheduled(H2WriteStage::Enter),
        }
    }

    /// Take the phase out, leaving the terminal one behind.
    ///
    /// `ConnectionH2::poll_write_target` calls this once per call and writes
    /// the next phase back before it returns, so the placeholder is never
    /// observed from outside that function. Taking the phase BY VALUE is
    /// what lets a scheduled step destructure [`H2ScheduledPass`] — the
    /// converter's `into_buffers` and the scheduler's `end_pass` both
    /// consume their arguments, and neither is callable through a `&mut`.
    pub(super) fn take_phase(&mut self) -> H2WritePhase {
        mem::replace(
            &mut self.phase,
            H2WritePhase::Unscheduled(H2WriteStage::Ended),
        )
    }
}
