//! Owned state of ONE [`super::h2::ConnectionH2::write_streams`] pass.
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
//! **This module does NOT own** the pass's control flow, nor four values one
//! pass touches. Each is excluded for its OWN reason, and they are not
//! interchangeable:
//!
//! - The scheduler's loaned `order` buffer and its
//!   [`super::h2_scheduler::ReadyIncrementalCensus`], handed out by
//!   `H2Scheduler::begin_pass` and given back to `H2Scheduler::end_pass`. Both
//!   are built AFTER the `expect_write` resume path has run, and that path can
//!   retire a stream through `ConnectionH2::remove_dead_stream` which
//!   `begin_pass` then does not enumerate — so building them at pass start
//!   would change which streams the pass visits.
//! - The [`super::converter::H2ConverterPass`]. It enumerates no streams, so
//!   the reason above does NOT apply to it; the reason is buffer stranding.
//!   Its constructor takes the three reusable scratch buffers out of
//!   `HpackState` (`take_converter_buf` / `take_lowercase_buf` /
//!   `take_cookie_buf`, each a `mem::take`), and only `into_buffers` after the
//!   per-stream loop hands them back through the matching `put_*` calls. The
//!   resume path's stall returns before both, so a converter built at pass
//!   start would be dropped on every stalled resume, leaving `HpackState`
//!   holding three empty `Vec`s to re-grow — the reuse the pass exists to
//!   provide, discarded on exactly the backpressure path that repeats most.
//! - The `Vec<IoSlice<'static>>`. This one IS built at pass start, ahead of the
//!   resume path and lent to it, so it is excluded by design rather than by
//!   necessity: `h2_transmit::gather` returns descriptors carrying a `'static`
//!   lifetime they do not have, pointing into `kawa.storage`, and
//!   `h2_transmit::confirm` must discharge them before the consume that may
//!   relocate that storage. Leaving the vector with the caller keeps that
//!   `unsafe` window exactly as wide as the three adjacent statements that open
//!   and close it, and stops a later poll/handle boundary from spanning it.
//!
//! The pass is a local threaded by `&mut`, never a field on `ConnectionH2`:
//! every exit of `write_streams` ends it, and the stalled-stream `break` falls
//! through to the tail, so it cannot outlive one call. Resumption ACROSS calls
//! is already expressed by `H2StreamTable::expect_write`.

use super::{GlobalStreamId, StreamId, parser::H2Error};

/// The `let` bindings of one `write_streams` pass, in the order the pre-image
/// declared them.
pub(super) struct H2WritePass {
    /// Connection-wide `(bytes, streams)` totals used to distribute the
    /// per-session overhead proportionally when a stream is recycled. Computed
    /// once at pass start by `ConnectionH2::compute_stream_byte_totals` and
    /// read by both the resume path and the scheduler loop.
    pub(super) byte_totals: (usize, usize),
    /// Whether the last flush ended on `FlushOutcome::Stalled`, i.e. whether
    /// `update_readiness_after_write` judged the socket unable to take more.
    /// Written by each flush and read immediately after it; the resume path's
    /// verdict is consumed before the scheduler loop can overwrite it.
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
        }
    }
}
