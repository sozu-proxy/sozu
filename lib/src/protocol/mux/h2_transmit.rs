//! Vectored transmit descriptor for one H2 stream's pending output, for
//! [`super::h2::ConnectionH2`].
//!
//! This is the sans-io output half of the write path, and it is deliberately
//! NOT one function. A pure `poll_transmit(&mut self, buf: &mut [u8])` cannot
//! serve this path: Sōzu's per-stream output already lives in `kawa.storage`
//! and is written by borrowing it, so filling a caller-supplied buffer would
//! introduce a copy the current code does not make. What this module exposes
//! instead is a **two-call protocol**:
//!
//! 1. [`gather`] borrows the stream's queued blocks as `IoSlice`s pointing
//!    straight into `kawa.storage`, and
//! 2. [`confirm`] applies the byte count the shell actually accepted.
//!
//! The shell performs the write between the two.
//!
//! **This is not `quinn-proto`'s `poll_transmit`, and the difference is not
//! cosmetic.** A QUIC datagram is all-or-nothing, so quinn's core can hand out
//! a transmit and forget it. A TLS byte stream accepts partial writes, so the
//! amount written is only known after the syscall and `kawa.consume(size)`
//! needs exactly that number. A confirm call is therefore structural, not an
//! accommodation. Sōzu's sibling UDP core (`protocol/udp/`) reaches for the
//! same idea at a coarser grain and does not call it `poll_transmit` either:
//! `UdpManager::poll_output` drains a manager-wide output queue, where
//! `gather` here sees one stream's `Kawa`. There is no `poll_transmit`
//! anywhere in this repository, and this module does not add one. The UDP
//! core's `Transmit` carries an owned `Vec<u8>` payload, which is the
//! opposite of what this path wants and must not be copied here.
//!
//! **This module owns**: gathering `kawa.out` into vectored descriptors, the
//! `unsafe` lifetime extension that makes those descriptors outlive the
//! borrow of `kawa`, and discharging that obligation before the consume.
//!
//! **What this split did and did not change.** The pre-image already cleared
//! the descriptors before the consume and already guarded it with the same
//! `debug_assert!`; the `unsafe` and that clear were six lines apart, not
//! eleven, with a push, three braces and the vectored write between them —
//! the debug event, byte counters and READABLE re-arm all came *after* the
//! clear. One of those moved: the caller now pushes its `DebugEvent::SocketIO`
//! *before* calling `confirm`, so the clear follows the debug event instead of
//! preceding it. That is inert — `debug.push` does not touch `kawa` — and the
//! obligation this module states is "clear before the consume", which still
//! holds because both now happen inside `confirm`, in that order.
//! So this is not a correctness fix and it does not make anything
//! type-level: two free functions each taking an independent `&mut Vec` is
//! co-location, not enforcement, and a caller can still call `Kawa::consume`
//! without ever asking this module. A guard type owning the vector would earn
//! the word "structural"; this does not. What it does buy is real but modest:
//! one place to read the obligation and its discharge, a `gather` reusable
//! from a second call site, and both halves drivable in a unit test over a
//! `SliceBuffer` with no pool and no socket — which is how the cases below
//! exist at all.
//!
//! **It deliberately does NOT own, and why**:
//!
//! - **The socket.** The whole point is that the write happens outside. The
//!   caller passes the gathered slices to whatever `SocketHandler` it holds
//!   and reports the result back.
//! - **Readiness, metrics and byte counters.** [`confirm`] returns nothing but
//!   the consume; stall classification stays with `update_readiness_after_write`
//!   in `mux/mod.rs`, which both H1 and H2 share, and the per-position byte
//!   counters stay on `Position`. Splitting a counter across two owners is how
//!   a gauge starts drifting.
//! - **Which stream goes next.** Ordering is
//!   [`super::h2_scheduler::H2Scheduler::begin_pass`]'s decision. This module
//!   sees one stream at a time and imposes no order of its own.
//!
//! # Fairness: a stalled pass is not fair to the streams it did not reach
//!
//! Worth stating here because this is where the two halves meet. The pass
//! order comes from the scheduler. `Prioriser::apply_incremental_rotation`
//! does rotate *every* same-urgency run's incremental tail — what LIFECYCLE
//! invariant 26 scopes to the leading bucket is the **commit**, and hence the
//! fairness bound. `Prioriser` holds one connection-global
//! `incremental_cursor` and `end_pass` commits only the first incremental
//! stream that fired, which is always in the lowest-numbered ready bucket. In
//! any other bucket that cursor is a foreign id range, so `partition_point`
//! returns a constant and the rotation, though it runs, is a no-op: that
//! bucket's tail is frozen. While every stream still gets its frame on a pass
//! that runs to completion, that is *positional* unfairness only.
//!
//! It becomes **byte** starvation the moment a pass is cut short. When
//! [`confirm`]'s caller sees a stalled socket and stops the pass at stream 5,
//! stream 7 is simply not written — and if the next pass presents the same
//! frozen order, it is not written again, pass after pass. The stall is where
//! a positional freeze turns into a stream that never sends. Do not read a
//! completed-pass fairness argument as covering a stalled one; it does not.
//!
//! This module cannot fix that — it sees one stream and has no order to
//! change — and it deliberately makes no claim to. The fix, if one is wanted,
//! is a per-bucket cursor in the scheduler.

use std::io::IoSlice;

use kawa::{AsBuffer, Kawa};

/// Gather the stream's queued output blocks into `io_slices` as vectored
/// descriptors borrowed directly out of `kawa.storage`, and return the total
/// number of bytes they describe.
///
/// Gathering stops at the first [`kawa::OutBlock::Delimiter`], matching what
/// the inline loop this replaced did: a delimiter marks a boundary the stream
/// must not write past in one go.
///
/// `io_slices` is cleared first and is the caller's reusable scratch — it is
/// hoisted once per write pass rather than per stream, so a pass over N
/// streams performs no additional allocation.
///
/// # Safety obligation taken on here, discharged in [`confirm`]
///
/// The returned descriptors carry a `'static` lifetime they do not truly
/// have: they point into `kawa.storage`, which [`Kawa::consume`] may relocate
/// via `ptr::copy`. They are valid only until the next mutation of `kawa`.
/// **Every gather must be followed by a [`confirm`] on the same `io_slices`
/// before anything else touches `kawa`** — `confirm` clears the vector before
/// it consumes, so no extended reference is live across the relocation.
///
/// Pairing the two in one module is the point of this split: before it, the
/// `unsafe` and the `io_slices.clear()` that discharges it sat eleven lines
/// apart inside a loop body that also did metrics, readiness and stall
/// classification.
pub(super) fn gather<T: AsBuffer>(kawa: &Kawa<T>, io_slices: &mut Vec<IoSlice<'static>>) -> usize {
    io_slices.clear();
    let buffer = kawa.storage.buffer();
    let mut bytes_offered = 0usize;
    for block in kawa.out.iter() {
        match block {
            kawa::OutBlock::Delimiter => break,
            kawa::OutBlock::Store(store) => {
                let data = store.data(buffer);
                // SAFETY: the IoSlice references point into kawa's storage
                // buffer. They are used only for the caller's vectored write
                // and are cleared by `confirm` immediately after, before
                // `kawa.consume()` which may relocate the buffer via
                // `ptr::copy` (shift). No dangling 'static refs exist during
                // consume().
                let data: &'static [u8] =
                    unsafe { std::slice::from_raw_parts(data.as_ptr(), data.len()) };
                bytes_offered += data.len();
                io_slices.push(IoSlice::new(data));
            }
        }
    }
    debug_assert_eq!(
        io_slices.iter().map(|s| s.len()).sum::<usize>(),
        bytes_offered,
        "the reported offer must equal the bytes the descriptors describe"
    );
    // Pair (negative space): a non-zero offer implies at least one
    // descriptor — a caller handed a positive count with nothing to write
    // would advance the stream by bytes that were never sent. The converse
    // is deliberately NOT asserted: a zero-length `Store` yields a descriptor
    // that offers no bytes, which the pre-image handled without complaint and
    // an `assert_eq!` on the two emptinesses would have turned into a panic.
    debug_assert!(
        bytes_offered == 0 || !io_slices.is_empty(),
        "a non-zero offer must be backed by at least one descriptor"
    );
    bytes_offered
}

/// Apply the byte count the shell accepted, and discharge [`gather`]'s safety
/// obligation in the same step.
///
/// `size` is what the socket reported, which may be less than the gather
/// offered (a partial write), zero (`WouldBlock`), or the whole offer. All
/// three are ordinary: the caller classifies them through
/// `update_readiness_after_write`; this function only advances the stream.
///
/// The clear happens BEFORE the consume, and that ordering is the whole
/// safety argument — see [`gather`].
pub(super) fn confirm<T: AsBuffer>(
    kawa: &mut Kawa<T>,
    io_slices: &mut Vec<IoSlice<'static>>,
    size: usize,
) {
    // Discharge the obligation `gather` took on, before the consume that may
    // relocate the buffer underneath those references.
    io_slices.clear();
    debug_assert!(
        io_slices.is_empty(),
        "IoSlice refs must be cleared before consume"
    );
    kawa.consume(size);
}

#[cfg(test)]
mod tests {
    use super::*;
    use kawa::{Buffer, Kind, SliceBuffer, Store};

    /// Build a kawa whose `out` queue holds `blocks`, each a slice of the
    /// backing storage, so `gather` has something to describe.
    fn kawa_with_out<'a>(
        buf: &'a mut [u8],
        payload: &[u8],
        splits: &[usize],
    ) -> Kawa<SliceBuffer<'a>> {
        let mut kawa = Kawa::new(Kind::Response, Buffer::new(SliceBuffer(buf)));
        kawa.storage.space()[..payload.len()].copy_from_slice(payload);
        kawa.storage.fill(payload.len());
        // `Kawa::consume` falls back to `storage.head` for `leftmost_ref()`
        // once `out` drains, and subtracts `storage.start` from it. In
        // production `prepare()` advances `head` past everything it emitted;
        // a fixture that leaves it at 0 underflows on the last consume. Model
        // what production does rather than working around the symptom.
        kawa.storage.head = payload.len();
        let mut start = 0usize;
        for &end in splits {
            kawa.out
                .push_back(kawa::OutBlock::Store(Store::Slice(kawa::repr::Slice {
                    start: start as u32,
                    len: (end - start) as u32,
                })));
            start = end;
        }
        if start < payload.len() {
            kawa.out
                .push_back(kawa::OutBlock::Store(Store::Slice(kawa::repr::Slice {
                    start: start as u32,
                    len: (payload.len() - start) as u32,
                })));
        }
        kawa
    }

    fn gathered_bytes(io_slices: &[IoSlice<'static>]) -> Vec<u8> {
        io_slices.iter().flat_map(|s| s.to_vec()).collect()
    }

    #[test]
    fn gather_describes_every_queued_block_in_order() {
        let mut buf = vec![0u8; 256];
        let payload = b"abcdefghijklmnop";
        let mut kawa = kawa_with_out(&mut buf, payload, &[4, 9]);
        let mut slices: Vec<IoSlice<'static>> = Vec::new();

        let offered = gather(&kawa, &mut slices);

        assert_eq!(offered, payload.len());
        assert_eq!(slices.len(), 3, "three blocks, three descriptors");
        assert_eq!(gathered_bytes(&slices), payload.to_vec());
        confirm(&mut kawa, &mut slices, offered);
    }

    #[test]
    fn gather_on_an_empty_queue_offers_nothing() {
        let mut buf = vec![0u8; 64];
        let mut kawa = Kawa::new(Kind::Response, Buffer::new(SliceBuffer(&mut buf)));
        let mut slices: Vec<IoSlice<'static>> = Vec::new();

        assert_eq!(gather(&kawa, &mut slices), 0);
        assert!(slices.is_empty());
        confirm(&mut kawa, &mut slices, 0);
    }

    #[test]
    fn gather_stops_at_a_delimiter() {
        let mut buf = vec![0u8; 256];
        let payload = b"abcdefgh";
        let mut kawa = Kawa::new(Kind::Response, Buffer::new(SliceBuffer(&mut buf)));
        kawa.storage.space()[..payload.len()].copy_from_slice(payload);
        kawa.storage.fill(payload.len());
        kawa.out
            .push_back(kawa::OutBlock::Store(Store::Slice(kawa::repr::Slice {
                start: 0,
                len: 4,
            })));
        kawa.out.push_back(kawa::OutBlock::Delimiter);
        kawa.out
            .push_back(kawa::OutBlock::Store(Store::Slice(kawa::repr::Slice {
                start: 4,
                len: 4,
            })));
        let mut slices: Vec<IoSlice<'static>> = Vec::new();

        let offered = gather(&kawa, &mut slices);

        assert_eq!(offered, 4, "the delimiter bounds the offer");
        assert_eq!(gathered_bytes(&slices), b"abcd".to_vec());
        confirm(&mut kawa, &mut slices, 0);
    }

    /// `confirm` leaves the descriptor vector empty, so the next round's
    /// [`gather`] cannot append to stale references.
    ///
    /// **This test does NOT pin the clear-before-consume ORDERING, and an
    /// earlier version of it claimed to.** It observes `io_slices` only after
    /// `confirm` has returned, and both orderings leave the vector empty
    /// there, so it is structurally blind to the swap. Moving the clear after
    /// the consume and deleting the internal `debug_assert!` leaves this test
    /// green. It was "seen red" by the revert — but the assertion that fired
    /// was the production `debug_assert!`, not this one, which is a way for
    /// the red-then-green ritual to pass while proving nothing.
    ///
    /// The ordering's real guard is the `debug_assert!` inside [`confirm`],
    /// which runs in every test, e2e and dev build and compiles out in
    /// release. Note it is a POSITIONAL check — it asserts the vector is empty
    /// where it stands, immediately before the consume — so it fires only if
    /// the clear is moved past it, and says nothing if clear and assert are
    /// moved together. TO SEE IT RED: in [`confirm`], move `io_slices.clear();`
    /// below `kawa.consume(size);` and leave the assert where it is. Measured:
    /// six tests fail, all with `IoSlice refs must be cleared before consume`.
    /// Move the assert too and none do.
    ///
    /// TO SEE *THIS* TEST RED: delete the `io_slices.clear();` line from
    /// [`confirm`] entirely. It fails with
    /// `confirm must leave no descriptor for the next round`.
    #[test]
    fn confirm_leaves_no_descriptor_for_the_next_round() {
        let mut buf = vec![0u8; 256];
        let payload = b"abcdefgh";
        let mut kawa = kawa_with_out(&mut buf, payload, &[4]);
        let mut slices: Vec<IoSlice<'static>> = Vec::new();

        gather(&kawa, &mut slices);
        assert!(!slices.is_empty(), "premise: the gather produced something");

        confirm(&mut kawa, &mut slices, 4);

        assert!(
            slices.is_empty(),
            "confirm must leave no descriptor for the next round"
        );
    }

    // ── Property coverage: multi-round drives across delimiters and stalls ─
    //
    // A sibling of `h2.rs`'s `write_pass_property` and `reassembly_property`
    // and `h2_scheduler.rs`'s `fairness_property`, not a case inside any of
    // them: those drive the HPACK encoder across a write pass, the
    // CONTINUATION accumulator across a read pass, and the scheduler's cursor
    // across many passes.
    //
    // Its justification is coverage, not a distinct oracle alone. The four
    // deterministic cases above are all SINGLE-ROUND. This drives the
    // gather/confirm pair round after round over an arbitrary block split,
    // arbitrary interleaved `Delimiter`s, and an arbitrary cycle of shell
    // accepts **including zero** — which is production's `WouldBlock` and the
    // only way to reach the stalled pass this module's header discusses. An
    // earlier version generated accepts of `1 + arbitrary % len`, so the
    // stall was structurally ungenerable and the header described a path no
    // test could enter.
    //
    // `drive` mirrors `flush_stream_out`'s loop rather than approximating it:
    // it stops on a zero accept against a non-zero offer, which is exactly
    // what `update_readiness_after_write` reports as `FlushOutcome::Stalled`.
    // An earlier version broke on `offered == 0` instead, which production
    // never does — that reading would skip a leading `Delimiter` rather than
    // consuming it.
    //
    // The oracle is BYTES AT A FIXED ORDER and says nothing about liveness or
    // turn-taking across streams. It must not: the pass order is the
    // scheduler's, invariant 26 bounds fairness only within the leading
    // bucket, and a stalled pass is not fair to the streams it did not reach.
    // A property asserting every stream drains would assert something this
    // repository does not guarantee.
    mod partial_write_property {
        use super::*;
        use quickcheck::{Arbitrary, Gen, TestResult, quickcheck};

        /// Hard bound on drive rounds. Every round either moves bytes or pops
        /// a `Delimiter`, so termination is provable; the bound is asserted
        /// as a postcondition rather than assumed from green runs.
        const MAX_ROUNDS: usize = 512;

        #[derive(Clone, Debug)]
        struct WritePlan {
            payload: Vec<u8>,
            /// Byte offsets at which the payload is cut into blocks.
            block_splits: Vec<usize>,
            /// Block indices after which a `Delimiter` is inserted.
            delimiter_after: Vec<usize>,
            /// Bytes the shell accepts per round, cycled. May contain 0.
            chunk_sizes: Vec<usize>,
        }

        impl Arbitrary for WritePlan {
            fn arbitrary(g: &mut Gen) -> Self {
                let len = 1 + usize::arbitrary(g) % 64;
                let payload: Vec<u8> = (0..len).map(|i| (i % 251) as u8).collect();
                let block_count = 1 + usize::arbitrary(g) % 4;
                let mut block_splits: Vec<usize> = (0..block_count.saturating_sub(1))
                    .map(|_| 1 + usize::arbitrary(g) % len)
                    .collect();
                block_splits.sort_unstable();
                block_splits.dedup();
                block_splits.retain(|&s| s < len);
                let delimiter_after: Vec<usize> = (0..usize::arbitrary(g) % 3)
                    .map(|_| usize::arbitrary(g) % (block_splits.len() + 1))
                    .collect();
                // Deliberately includes 0: a zero accept is WouldBlock, and it
                // is the only input that reaches the stalled pass.
                let chunk_sizes: Vec<usize> = (0..1 + usize::arbitrary(g) % 4)
                    .map(|_| usize::arbitrary(g) % (len + 1))
                    .collect();
                WritePlan {
                    payload,
                    block_splits,
                    delimiter_after,
                    chunk_sizes,
                }
            }
        }

        fn kawa_for<'a>(buf: &'a mut [u8], plan: &WritePlan) -> Kawa<SliceBuffer<'a>> {
            let payload = &plan.payload;
            let mut kawa = Kawa::new(Kind::Response, Buffer::new(SliceBuffer(buf)));
            kawa.storage.space()[..payload.len()].copy_from_slice(payload);
            kawa.storage.fill(payload.len());
            kawa.storage.head = payload.len();
            let mut bounds: Vec<usize> = plan.block_splits.clone();
            bounds.push(payload.len());
            let mut start = 0usize;
            for (index, &end) in bounds.iter().enumerate() {
                if end > start {
                    kawa.out
                        .push_back(kawa::OutBlock::Store(Store::Slice(kawa::repr::Slice {
                            start: start as u32,
                            len: (end - start) as u32,
                        })));
                    start = end;
                }
                if plan.delimiter_after.contains(&index) {
                    kawa.out.push_back(kawa::OutBlock::Delimiter);
                }
            }
            kawa
        }

        /// Returns the bytes handed to the shell, and whether the queue
        /// drained (false means the pass stalled, production's
        /// `FlushOutcome::Stalled`).
        fn drive(plan: &WritePlan) -> (Vec<u8>, bool) {
            let mut buf = vec![0u8; plan.payload.len() * 4 + 64];
            let mut kawa = kawa_for(&mut buf, plan);
            let mut slices: Vec<IoSlice<'static>> = Vec::new();
            let mut written: Vec<u8> = Vec::new();
            let mut tick = 0usize;
            let mut rounds = 0usize;
            while !kawa.out.is_empty() {
                rounds += 1;
                assert!(
                    rounds <= MAX_ROUNDS,
                    "drive must terminate within its bound"
                );
                let offered = gather(&kawa, &mut slices);
                let accept = plan.chunk_sizes[tick % plan.chunk_sizes.len()].min(offered);
                tick += 1;
                let mut taken = 0usize;
                for slice in slices.iter() {
                    if taken >= accept {
                        break;
                    }
                    let take = (accept - taken).min(slice.len());
                    written.extend_from_slice(&slice[..take]);
                    taken += take;
                }
                confirm(&mut kawa, &mut slices, accept);
                // Production's stall: the shell accepted nothing while bytes
                // were pending. A zero OFFER is not a stall — that is a
                // leading `Delimiter`, which `consume(0)` pops.
                if accept == 0 && offered > 0 {
                    return (written, false);
                }
            }
            (written, true)
        }

        // Whatever the block split, the delimiter placement and the accept
        // pattern, the bytes handed to the shell are a prefix of the payload —
        // never reordered, duplicated or skipped — and they are the WHOLE
        // payload exactly when the queue drained.
        //
        // TO SEE THIS RED: in `gather`, change `for block in kawa.out.iter()`
        // to `for block in kawa.out.iter().skip(1)`. Measured: five tests fail
        // in total — this one, `gather_describes_every_queued_block_in_order`
        // and `gather_stops_at_a_delimiter` here, plus
        // `one_write_pass_prefixes_its_first_header_block_only_and_shares_one_encoder`
        // and `qc_one_write_pass_prefixes_its_first_header_block_only` in
        // `h2.rs`. I could not construct a mutation that reddens this property
        // and nothing else; its value is the input space it covers, not
        // exclusive detection, and that is stated here rather than implied.
        quickcheck! {
            fn qc_partial_writes_preserve_the_byte_stream(plan: WritePlan) -> TestResult {
                if plan.payload.is_empty() || plan.chunk_sizes.is_empty() {
                    return TestResult::discard();
                }
                let (written, drained) = drive(&plan);
                if written.len() > plan.payload.len()
                    || written != plan.payload[..written.len()]
                {
                    return TestResult::error(
                        "the bytes handed to the shell are not a prefix of the payload",
                    );
                }
                if drained && written != plan.payload {
                    return TestResult::error(
                        "the queue drained but the payload was not fully written",
                    );
                }
                TestResult::passed()
            }
        }
    }
}
