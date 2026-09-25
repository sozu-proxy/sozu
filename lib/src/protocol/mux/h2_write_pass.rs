//! Owned state of ONE [`super::h2::H2Shell::write_streams`] pass.
//!
//! Every value here used to be a `let` inside that function. They are grouped
//! into one struct because the write path is being turned into a drive loop —
//! a `poll` that names the next transmit, a shell that performs it, a `handle`
//! that reports it back — and a value the pre-image kept in a stack slot
//! across the flush loop has to survive that round trip. Extracting the state
//! first, with the control flow left exactly as it was, keeps the two halves
//! separately reviewable: this half moves no condition, no ordering and no
//! saturating-add boundary.
//!
//! **This module owns**: the byte counters, flags and scratch vectors of one
//! write pass, and their initial values.
//!
//! **This module also owns**, since the inversion, the pass's PHASE
//! ([`H2WritePhase`]) and the three values the drive loop has to carry across
//! the caller boundary without being able to build them at pass start: the
//! [`super::converter::H2ConverterPass`], the scheduler's loaned `order`
//! buffer and its [`super::h2_scheduler::ReadyIncrementalCensus`]. They are
//! LATE-INITIALISED — see [`H2WritePass::adopt_scheduler_pass`] — and the two
//! reasons they cannot be pass-start fields are different reasons, not one:
//!
//! - `order` and `census` are built by `H2Scheduler::begin_pass` and given
//!   back to `H2Scheduler::end_pass`. `begin_pass` enumerates
//!   `stream_table.streams()`, and the `expect_write` resume path that runs
//!   BEFORE it can retire a stream out of that same map through
//!   `ConnectionH2::remove_dead_stream` — so building them at pass start would
//!   change which streams the pass visits.
//! - The `H2ConverterPass` enumerates no streams, so the reason above does NOT
//!   apply to it; its reason is buffer stranding. Its constructor takes the
//!   three reusable scratch buffers out of `HpackState` (`take_converter_buf`
//!   / `take_lowercase_buf` / `take_cookie_buf`, each a `mem::take`), and only
//!   `into_buffers` hands them back through the matching `put_*` calls. The
//!   resume path's stall ends the pass before both, so a converter built at
//!   pass start would be dropped on every stalled resume, leaving `HpackState`
//!   holding three empty `Vec`s to re-grow — the reuse the pass exists to
//!   provide, discarded on exactly the backpressure path that repeats most.
//!
//! **This module does NOT own** the pass's control flow — that is
//! `ConnectionH2::poll_write_target` — nor the `Vec<IoSlice<'static>>`. The
//! vector IS buildable at pass start and is still excluded, by design rather
//! than by necessity: `h2_transmit::gather` returns descriptors carrying a
//! `'static` lifetime they do not have, pointing into `kawa.storage`, and
//! `h2_transmit::confirm` must discharge them before the consume that may
//! relocate that storage. Leaving the vector with the shell keeps that
//! `unsafe` window exactly as wide as the three adjacent statements that open
//! and close it, and stops the poll/handle boundary from spanning it.
//!
//! The pass is a local threaded by `&mut`, never a field on `ConnectionH2`:
//! every exit of `write_streams` ends it, and every phase transition falls
//! through to [`H2WritePhase::End`] or to a `Done`, so it cannot outlive one
//! call. Resumption ACROSS calls is already expressed by
//! `H2StreamTable::expect_write`.

use super::{
    GlobalStreamId, StreamId, StreamState, converter::H2ConverterPass,
    h2_scheduler::ReadyIncrementalCensus, parser::H2Error,
};

/// What every late-initialised accessor below panics with.
///
/// ONE message and ONE `expect` per accessor, so a violation names the
/// invariant rather than a line number: the three scheduler-pass values are
/// absent in [`H2WritePhase::Start`], [`H2WritePhase::Resume`] and
/// [`H2WritePhase::Ended`], present from the transition into
/// [`H2WritePhase::Prepare`], and taken back at the first statement of
/// [`H2WritePhase::End`], which then moves the pass to `Ended`.
///
/// Reaching this message means an accessor ran in one of those three phases.
/// `Ended` exists so that a re-entry after the pass answered cannot be one of
/// them: `End` releases into `None`, so without a separate terminal phase a
/// second poll would re-enter `End` and `expect` on an empty `Option`.
const SCHEDULER_PASS_EXPECT: &str = "the converter, order and census are adopted at the transition into \
     H2WritePhase::Prepare and released at the first statement of \
     H2WritePhase::End; only Start, Resume and Ended may observe them absent";

/// Where in its pass [`ConnectionH2::poll_write_target`] is, and the only
/// thing that tells a re-entry after a transmit apart from a first entry.
///
/// [`ConnectionH2::poll_write_target`]: super::h2::ConnectionH2::poll_write_target
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum H2WritePhase {
    /// Nothing has been examined yet. The next poll reads `expect_write` and
    /// goes to either [`Self::Resume`] or, through
    /// [`H2WritePass::adopt_scheduler_pass`], to [`Self::Prepare`].
    ///
    /// A fifth variant the pre-image had no room for: the parked-stream
    /// decision needs `&mut ConnectionH2`, which [`H2WritePass::new`] does not
    /// have, so it cannot be taken when the pass is built.
    Start,
    /// Flushing the stream `expect_write` parked on a previous pass. Drains
    /// or stalls, and never enters the scheduler loop without draining first.
    Resume {
        stream_id: StreamId,
        gid: GlobalStreamId,
        /// `Some(amount)` when the SAME stream is also parked waiting for read
        /// buffer space, so a drain that frees `amount` bytes re-arms
        /// `Ready::READABLE`. Computed once, before the first transmit.
        cross_read_amount: Option<usize>,
        /// `Stream::state` as it was BEFORE the flush, which is when the
        /// pre-image read it. Carried rather than re-read afterwards because
        /// its consumer, `ConnectionH2::handle_1xx_reset`, decides whether to
        /// re-arm `Ready::READABLE` on the linked token: a stream retired
        /// mid-flush must not change which state that check sees.
        stream_state: StreamState,
    },
    /// About to run the eligibility gate, `kawa.prepare`, the window debit and
    /// `census.note_fired(urgency, ...)` for `order[cursor]`. Entered ONCE per stream: the
    /// round-again after a partial write re-enters [`Self::Flush`], never
    /// this, which is what keeps one pass to one prepare per stream.
    Prepare { cursor: usize },
    /// `order[cursor]` is prepared; only the flush remains. Re-entered after
    /// every transmit, and the place the pre-image's
    /// `while !kawa.out.is_empty()` now lives as a resumption state.
    Flush {
        cursor: usize,
        stream_id: StreamId,
        gid: GlobalStreamId,
        /// `Stream::state` as it was before the flush — see
        /// [`Self::Resume::stream_state`] for why the timing matters.
        stream_state: StreamState,
        /// `H2Scheduler::priority`'s answer for this stream, read once in
        /// [`Self::Prepare`] exactly as the pre-image read it once per loop
        /// iteration. Re-reading it here would be a second `HashMap` lookup
        /// per stream per pass on the write hot path.
        urgency: u8,
        is_incremental: bool,
    },
    /// The scheduler loop is over: release the scheduler-pass values, retire,
    /// account, and hand the pass's verdict back to the shell, which moves the
    /// pass to [`Self::Ended`] on the way out.
    End,
    /// The pass has already answered. It holds no scheduler-pass value and can
    /// produce no further work.
    ///
    /// A terminal phase rather than leaving [`Self::End`] in place: that arm
    /// takes the converter, order and census out of the pass as its FIRST
    /// statement, so re-entering it would `expect` on three empty `Option`s.
    /// `H2Shell::write_streams` never re-polls — it returns on `Done` and
    /// on `Finalize` — but `poll_write_target` is `pub`, so its callers are no
    /// longer enumerable by reading this crate, and the read side's
    /// `poll_read_target` already has direct unit tests, so a future caller
    /// driving this core by hand is the likely one to find out.
    Ended,
}

/// The `let` bindings of one `write_streams` pass, in the order the pre-image
/// declared them, plus the phase and the three late-initialised values the
/// inversion has to carry across the poll/handle boundary.
pub struct H2WritePass {
    /// Where the drive loop is. Every transition is made by
    /// `ConnectionH2::poll_write_target` and by nothing else.
    pub(super) phase: H2WritePhase,
    /// Connection-wide `(bytes, streams)` totals used to distribute the
    /// per-session overhead proportionally when a stream is recycled. Computed
    /// once at pass start by `ConnectionH2::compute_stream_byte_totals` and
    /// read by both the resume path and the scheduler loop.
    pub(super) byte_totals: (usize, usize),
    /// Whether the last transmit left the socket unable to take more, i.e.
    /// `update_readiness_after_write`'s verdict — the ONLY place the write
    /// core records a socket answer, and the sole input to the round-again
    /// decision. Written by `ConnectionH2::handle_write` and read by
    /// `ConnectionH2::poll_write_target`.
    ///
    /// Cleared at the top of every [`H2WritePhase::Prepare`], because the
    /// pre-image re-assigned it from each `flush_stream_out` call and a stream
    /// whose queue is empty flushed zero rounds and therefore read NOT
    /// stalled. Left sticky, one stream's stall would silently terminate the
    /// next stream's flush before its first transmit.
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
    /// The pass's HPACK converter, holding the three scratch buffers it took
    /// out of `HpackState`. `None` until the scheduler pass opens, and `None`
    /// again from the moment `H2WritePhase::End` releases it.
    converter: Option<H2ConverterPass>,
    /// The scheduler's loaned ordering buffer for this pass, walked by cursor.
    order: Option<Vec<StreamId>>,
    /// The pass's same-urgency ready-peer census, returned to
    /// `H2Scheduler::end_pass` with [`Self::order`].
    census: Option<ReadyIncrementalCensus>,
}

/// A tripwire, not the guarantee.
///
/// The guarantee that the converter's three pooled buffers reach
/// `HpackState` again is structural: between
/// `Self::adopt_scheduler_pass` and the first statement of
/// `H2WritePhase::End`, `ConnectionH2::poll_write_target` has exactly ONE
/// `return` — the `Transmit` yield — and `H2Shell::write_streams`' drive
/// loop answers every `Transmit` and leaves only on `Done` or `Finalize`. So
/// no pass can end while `Self::converter` is `Some`.
///
/// This catches the one way that could stop being true: a future caller of the
/// now-`pub` `poll_write_target` that stops driving after a `Transmit` — a
/// caller this crate no longer enumerates by reading its own call sites. The
/// other way — a caller that polls once MORE after the pass answered — is
/// closed by `H2WritePhase::Ended` instead, and needs no tripwire because
/// the converter is already `None` by then.
/// Dropping the converter there loses the buffers permanently and silently —
/// `HpackState` simply re-grows three `Vec`s on every pass from then on, with
/// no error and no metric. `debug_assert!` puts that in the test suite's reach
/// instead; `thread::panicking()` keeps a real test failure legible rather
/// than aborting the process on a double panic.
impl Drop for H2WritePass {
    fn drop(&mut self) {
        debug_assert!(
            std::thread::panicking() || self.converter.is_none(),
            "a write pass ended holding its converter: the three HPACK scratch \
             buffers it took out of HpackState are about to be dropped instead \
             of returned. A caller stopped driving poll_write_target after an \
             H2WriteTarget::Transmit."
        );
    }
}

impl H2WritePass {
    /// Begin a pass over `byte_totals`, the connection-wide
    /// `(bytes, streams)` pair the recycle path distributes overhead against.
    ///
    /// Every other field starts empty or zero, exactly as its pre-image `let`
    /// did. Hoisting those initialisations to pass start is inert: an empty
    /// `Vec` allocates nothing, and nothing reads any of them before the
    /// statement that used to declare them.
    pub fn new(byte_totals: (usize, usize)) -> Self {
        Self {
            phase: H2WritePhase::Start,
            byte_totals,
            stalled: false,
            resume_bytes: 0,
            completed_streams: Vec::new(),
            socket_write: false,
            total_bytes_written: 0,
            freshly_emitted_rsts: Vec::new(),
            consumed: 0,
            stream_bytes: 0,
            converter: None,
            order: None,
            census: None,
        }
    }

    /// Open the scheduler half of the pass, taking ownership of the three
    /// values that could not be built when the pass was.
    ///
    /// Called exactly once, at the transition into [`H2WritePhase::Prepare`],
    /// from either [`H2WritePhase::Start`] (nothing was parked) or
    /// [`H2WritePhase::Resume`] (the parked stream drained). Everything after
    /// it may use the accessors below; nothing before it may.
    pub(super) fn adopt_scheduler_pass(
        &mut self,
        converter: H2ConverterPass,
        order: Vec<StreamId>,
        census: ReadyIncrementalCensus,
    ) {
        debug_assert!(
            self.converter.is_none() && self.order.is_none() && self.census.is_none(),
            "a write pass opened its scheduler pass twice"
        );
        self.converter = Some(converter);
        self.order = Some(order);
        self.census = Some(census);
    }

    /// Close the scheduler half of the pass and hand its three values back.
    ///
    /// Called exactly once, as the FIRST statement of [`H2WritePhase::End`] —
    /// before any `return` that arm can take, which is what makes the
    /// converter's buffers reach `HpackState` on the close-frontend GOAWAY
    /// exit. The MadeYouReset cap trip still drops them, because it returns
    /// between `H2ConverterPass::into_buffers` and the `put_*` calls exactly
    /// as the pre-image did. The caller moves the pass to
    /// [`H2WritePhase::Ended`] in the same breath, so "exactly once" is
    /// enforced by the phase and not only by inspection.
    pub(super) fn release_scheduler_pass(
        &mut self,
    ) -> (H2ConverterPass, Vec<StreamId>, ReadyIncrementalCensus) {
        (
            self.converter.take().expect(SCHEDULER_PASS_EXPECT),
            self.order.take().expect(SCHEDULER_PASS_EXPECT),
            self.census.take().expect(SCHEDULER_PASS_EXPECT),
        )
    }

    /// The pass's HPACK converter, for the ONE `kawa.prepare` of one stream.
    pub(super) fn converter_mut(&mut self) -> &mut H2ConverterPass {
        self.converter.as_mut().expect(SCHEDULER_PASS_EXPECT)
    }

    /// The scheduler's ordering for this pass. The cursor in
    /// [`H2WritePhase::Prepare`] indexes exactly this slice, so a cursor past
    /// its end is what ends the scheduler loop.
    pub(super) fn order(&self) -> &[StreamId] {
        self.order.as_deref().expect(SCHEDULER_PASS_EXPECT)
    }

    /// The pass's same-urgency ready-peer census, read for
    /// `incremental_peer_count` and written by `note_fired` /
    /// `note_ineligible` — both of which take the urgency, so every count
    /// and every RFC 9218 §4 leader it holds is per bucket.
    pub(super) fn census_mut(&mut self) -> &mut ReadyIncrementalCensus {
        self.census.as_mut().expect(SCHEDULER_PASS_EXPECT)
    }
}
