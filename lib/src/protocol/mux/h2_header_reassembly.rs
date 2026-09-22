//! Owned accumulator for an in-progress HTTP/2 HEADERS+CONTINUATION field
//! block (RFC 9113 §4.3, §6.2, §6.10) for [`super::h2::ConnectionH2`].
//!
//! Before this module existed, `ConnectionH2::zero` played two incompatible
//! roles at once: the read-side accumulator a field block was reassembled
//! into across however many `readable()` passes a HEADERS+CONTINUATION
//! sequence took (`headers.header_block_fragment` — a `(start, len)`
//! [`kawa::repr::Slice`] window over `zero.storage`, extended in place), and
//! the write-side scratch buffer every control-frame flush (WINDOW_UPDATE,
//! RST_STREAM, GOAWAY) cleared and reused. LIFECYCLE.md invariant 24 has the
//! full account; four bugs shipped from the two roles sharing one buffer —
//! sozu-proxy/sozu#1396, #1397, #1401 and #1423 — the first three each
//! patched with their own `header_block_reassembly_in_progress()` guard at
//! their own clobber site, and #1423 found a fourth clobber site with no
//! guard at all.
//!
//! [`HeaderBlockAccumulator`] gives the read side a real owned lifetime
//! (`Vec<u8>`) instead of borrowing a window into `zero.storage`. A HEADERS
//! frame without END_HEADERS seeds it via [`HeaderBlockAccumulator::begin`]
//! with a copy of its own fragment (the same "copy out before the buffer
//! changes purpose" move the CVE-2024-27316 abort path in `h2.rs` already
//! made defensively, generalised to the happy path); every CONTINUATION
//! frame's payload is folded in via [`HeaderBlockAccumulator::append`] as
//! soon as it finishes being read, with `zero.storage` cleared by the caller
//! in the same step; the block is retired via [`HeaderBlockAccumulator::finish`]
//! — decoded on a completed block, or handed to
//! `DiscardedFieldBlock::Continuation` unread on a CVE-2024-27316 refusal —
//! either way returning ownership of the bytes and resetting to idle. Every
//! early-return path in `ConnectionH2::handle_headers_frame` that could
//! abandon an in-progress reassembly (e.g. an RFC 9113 §5.3.1 PRIORITY
//! self-dependency reset) must retire it before returning, or the
//! `is_in_progress()` flag leaks into the NEXT HEADERS frame processed on
//! this connection — which may belong to an entirely different stream —
//! and that frame's own "read from `zero.storage`" fast path is silently
//! skipped in favour of the leaked stream's stale bytes. This is not a
//! hypothetical: it shipped once, in this module's first changeset, and was
//! caught only by review (`a_priority_self_dependency_reset_does_not_leak_the_reassembly_accumulator`,
//! `h2.rs`) rather than by any assertion, because the accumulator's own
//! `begin()`/`append()`/`finish()` debug_asserts only fire on a call that
//! actually happens — a call that is *silently skipped* (the `if
//! !self.header_reassembly.is_in_progress() { begin(...) }` gate in the
//! `!end_headers` branch) asserts nothing at all.
//!
//! No write-side code holds a field named `header_reassembly` — that is
//! true in every build profile, and it is what makes a control-frame flush
//! unable to reach these bytes: not that misuse is impossible, but that no
//! write-side site currently NAMES the field to misuse. `begin`/`append`/
//! `finish`'s `debug_assert!`s only run in debug/test/e2e/fuzz builds; a
//! release build has none of them, so a second `begin()` while already
//! `in_progress` silently `clear()`s the accumulated history instead of
//! panicking — the exact #1396/#1397 failure mode, just relocated to this
//! type instead of `zero.storage` — and `append()`/`finish()` called while
//! idle silently grow or return meaningless bytes. B1 above is exactly
//! that: a same-module, `pub(super)` misuse the type could not have
//! prevented on its own, caught only because a human reviewed the code
//! that calls these methods. That is what closes the #1396/#1397/#1401
//! class for the write side, and is the majority of what closes #1423 —
//! but not the literal, narrow complaint #1423 was filed against, which is
//! answered below.
//!
//! **What is *not* eliminated, and still needs guarding** — checked via
//! `header_block_reassembly_in_progress()` at six call sites in `h2.rs`:
//! the frontend-hung-up-while-draining, WINDOW_UPDATE-drain and
//! RST_STREAM-drain stages plus the deferred-initial-GOAWAY-readiness check
//! (all four in `flush_pending_control_frames`), `graceful_goaway`'s own
//! defer-or-send decision, and `flush_zero_buffer`'s no-op guard: a single
//! CONTINUATION frame's payload can still be *mid-flight* in `zero.storage`
//! — a partial `socket_read()` that has not yet finished this one frame —
//! when a write pass runs in the same event-loop sweep. That window is real
//! (TCP segmentation can split any frame's payload across reads) and losing
//! those bytes there would still corrupt an otherwise-completable block.
//!
//! #1423 was filed against exactly one of those sites having no guard: the
//! frontend-hung-up-while-draining stage. The step that introduced this
//! module shipped that stage genuinely unguarded, reasoning that
//! `Ready::HUP` means "no further bytes can ever arrive" — that reasoning
//! is wrong. `Ready::HUP` is `is_read_closed() || is_write_closed()`
//! (`command/src/ready.rs`), and mio documents `is_read_closed()` as true
//! not only on a full close but also on a TCP half-close: a FIN with data
//! the peer already sent still sitting, unread, in the kernel receive
//! queue. `drive_frontend_shutdown_io` (`mod.rs`) force-calls `readable()`
//! for H2 on every `shutting_down()` poll, so a CONTINUATION frame split
//! across TCP segments landing a HUP event alongside its first segment is
//! the ordinary soft-stop path, not a contrived corner case — proven by
//! `a_continuation_frame_split_by_tcp_segmentation_survives_a_hup_while_draining`
//! (`h2.rs`), red on the unguarded stage with the exact
//! `StringDecodingError(NotEnoughOctets)` / decoder-desync failure shape
//! #1397/#1401 already showed. The frontend-hung-up-while-draining stage
//! now shares the same guard its three siblings already had, closing
//! #1423 for real.
//!
//! No dependency was added: this is a plain `Vec<u8>`, exactly what
//! `CONTRIBUTING.md`'s dependency policy and this step's task both require.

/// Owned accumulator for a HEADERS+CONTINUATION field block reassembly in
/// progress. `in_progress` is tracked independently of `bytes.is_empty()` so
/// a legitimately empty first fragment (e.g. a HEADERS frame carrying only
/// PRIORITY, no field-block bytes yet, with more CONTINUATION frames still
/// to come) cannot be mistaken for "no reassembly running" — see
/// `ConnectionH2::handle_headers_frame`'s buffer-selection branch, the one
/// caller that reads it.
pub(super) struct HeaderBlockAccumulator {
    bytes: Vec<u8>,
    in_progress: bool,
}

impl HeaderBlockAccumulator {
    pub(super) fn new() -> Self {
        Self {
            bytes: Vec::new(),
            in_progress: false,
        }
    }

    /// Whether a HEADERS+CONTINUATION block is currently being reassembled —
    /// i.e. whether [`Self::data`] describes the accumulated history of that
    /// block rather than being stale/idle.
    pub(super) fn is_in_progress(&self) -> bool {
        self.in_progress
    }

    /// Start reassembling a new block, seeding it with the initiating
    /// HEADERS frame's own field-block fragment (copied out of
    /// `zero.storage` by the caller before it hands the slice here).
    pub(super) fn begin(&mut self, fragment: &[u8]) {
        debug_assert!(
            !self.in_progress,
            "begin() called while a reassembly is already active"
        );
        self.bytes.clear();
        self.bytes.extend_from_slice(fragment);
        self.in_progress = true;
        self.debug_assert_invariants();
    }

    /// Append one CONTINUATION frame's payload to the block in progress.
    pub(super) fn append(&mut self, fragment: &[u8]) {
        debug_assert!(
            self.in_progress,
            "append() called with no reassembly in progress"
        );
        self.bytes.extend_from_slice(fragment);
        self.debug_assert_invariants();
    }

    /// The bytes accumulated so far. Meaningful only while
    /// [`Self::is_in_progress`] is true.
    pub(super) fn data(&self) -> &[u8] {
        &self.bytes
    }

    pub(super) fn len(&self) -> usize {
        self.bytes.len()
    }

    pub(super) fn is_empty(&self) -> bool {
        self.bytes.is_empty()
    }

    /// Retire the reassembly in progress — completed (about to be decoded)
    /// or abandoned (about to be discarded, CVE-2024-27316) — returning
    /// ownership of the accumulated bytes and resetting to idle. The single
    /// exit point from "in progress", matched by the single entry point
    /// [`Self::begin`].
    pub(super) fn finish(&mut self) -> Vec<u8> {
        debug_assert!(
            self.in_progress,
            "finish() called with no reassembly in progress"
        );
        self.in_progress = false;
        std::mem::take(&mut self.bytes)
    }

    #[cfg(debug_assertions)]
    fn check_invariants(&self) {
        // An idle accumulator holds no leftover bytes: `finish()` always
        // takes them, and `begin()` always clears before seeding.
        debug_assert!(
            self.in_progress || self.is_empty(),
            "an idle accumulator must hold no bytes"
        );
    }

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
    fn idle_by_default() {
        let acc = HeaderBlockAccumulator::new();
        assert!(!acc.is_in_progress());
        assert!(acc.is_empty());
        assert_eq!(acc.len(), 0);
        assert_eq!(acc.data(), &[] as &[u8]);
    }

    #[test]
    fn begin_seeds_and_marks_in_progress() {
        let mut acc = HeaderBlockAccumulator::new();
        acc.begin(b"hello");
        assert!(acc.is_in_progress());
        assert_eq!(acc.data(), b"hello");
        assert_eq!(acc.len(), 5);
    }

    #[test]
    fn append_extends_in_order() {
        let mut acc = HeaderBlockAccumulator::new();
        acc.begin(b"hello");
        acc.append(b", ");
        acc.append(b"world");
        assert_eq!(acc.data(), b"hello, world");
    }

    #[test]
    fn finish_returns_bytes_and_resets_to_idle() {
        let mut acc = HeaderBlockAccumulator::new();
        acc.begin(b"abc");
        acc.append(b"def");
        let out = acc.finish();
        assert_eq!(out, b"abcdef");
        assert!(!acc.is_in_progress());
        assert!(acc.is_empty());
    }

    #[test]
    fn begin_after_finish_starts_a_fresh_block() {
        let mut acc = HeaderBlockAccumulator::new();
        acc.begin(b"first-block");
        let first = acc.finish();
        assert_eq!(first, b"first-block");

        acc.begin(b"second-block");
        assert_eq!(acc.data(), b"second-block");
        let second = acc.finish();
        assert_eq!(second, b"second-block");
    }

    #[test]
    fn begin_with_an_empty_fragment_still_marks_in_progress() {
        // RFC 9113 permits a HEADERS frame whose field-block fragment is
        // empty (e.g. only PRIORITY, more CONTINUATION still to come). The
        // discriminator must be `in_progress`, not `is_empty()`.
        let mut acc = HeaderBlockAccumulator::new();
        acc.begin(b"");
        assert!(acc.is_in_progress());
        assert!(acc.is_empty());
        acc.append(b"later-bytes");
        assert_eq!(acc.data(), b"later-bytes");
    }

    #[test]
    #[should_panic(expected = "already active")]
    fn begin_while_in_progress_panics_in_debug() {
        let mut acc = HeaderBlockAccumulator::new();
        acc.begin(b"one");
        acc.begin(b"two");
    }

    #[test]
    #[should_panic(expected = "no reassembly in progress")]
    fn append_while_idle_panics_in_debug() {
        let mut acc = HeaderBlockAccumulator::new();
        acc.append(b"stray");
    }

    #[test]
    #[should_panic(expected = "no reassembly in progress")]
    fn finish_while_idle_panics_in_debug() {
        let mut acc = HeaderBlockAccumulator::new();
        acc.finish();
    }
}
