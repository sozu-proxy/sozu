//! The single, ordered output queue of one [`super::h2::ConnectionH2`].
//!
//! Every byte the connection sends that is not still owned by a stream goes
//! through here, in the order it must reach the peer: the control frames the
//! connection serialises (SETTINGS and its ACK, PING ACK, WINDOW_UPDATE,
//! RST_STREAM, GOAWAY, the client preface) and the unsent rest of the one
//! stream frame a partial write cut. Each entry is a WHOLE frame when it is
//! queued, so nothing that later removes a stream can leave a frame truncated
//! on the wire (sozu-proxy/sozu#1604).
//!
//! # Why the rest of a cut stream frame is copied, and nothing else is
//!
//! Stream frames stay zero-copy: `h2_transmit::gather_after` hands the
//! kernel `IoSlice`s pointing straight into the stream's `kawa.storage`,
//! behind whatever this queue holds. A frame is committed to the wire by the
//! write that sends its first byte. When that write stops inside the frame,
//! the frame's unsent rest is the only committed byte still owned by a
//! stream, and [`H2Output::adopt_tail`] moves it here, so the stream owns
//! only frames the wire has not started. Removing the stream then drops
//! whole unsent frames instead of the second half of one the peer is already
//! parsing, which keeps the FRAMING intact.
//!
//! It does not make those frames side-effect free. A HEADERS/CONTINUATION
//! block the stream still owns was already HPACK-encoded, and encoding it
//! changed the connection's encoder table. Dropping it unsent is repaired
//! outside this queue: the next write pass resets the encoder's table
//! (`ConnectionH2::reset_encoder_table`, `lib/src/protocol/mux/h2.rs`;
//! sozu-proxy/sozu#1627).
//!
//! The rest runs to the end of the frame, extended through the CONTINUATION
//! frames of an unfinished header block: RFC 9113 §6.10 forbids any other
//! frame between a HEADERS without END_HEADERS and its last CONTINUATION.
//!
//! The copy is bounded by one frame or one header block and only happens on a
//! partial write. The two reference implementations reach the same guarantee
//! by owning the in-flight bytes:
//!
//! - HAProxy copies every frame into its connection output ring (`mbuf`,
//!   `src/mux_h2.c`: `h2c_ack_settings` and `h2c_send_ping` append with
//!   `br_tail`); `h2s_make_data`'s zero-copy path instead swaps the stream's
//!   whole buffer into the ring, so the ring owns the frame either way, and
//!   it falls back to `goto copy` when the swap cannot apply.
//! - `h2` (hyperium) keeps one `FramedWrite` buffer for control frames and
//!   frame heads, and holds a large DATA payload by value in `Encoder::next`
//!   (`src/codec/framed_write.rs`) until it is written; `has_capacity` is
//!   false meanwhile, so no other frame can be queued behind a half-sent one.
//!   Its payload is a reference-counted `Buf`, which keeps it alive for free.
//!
//! Sōzu's stream storage is a pool buffer that a removed stream gives back,
//! with no reference count to keep committed bytes alive, so it takes the
//! `h2` shape with a copy of the in-flight rest instead of a reference.
//!
//! # Accounting
//!
//! The adopted rest is counted to its stream when it is adopted: by the time
//! it is written the stream may be gone. [`H2Output::consume`] therefore
//! answers only the bytes that were NOT prepaid that way, which are the
//! connection's own overhead.

use kawa::{AsBuffer, Kawa};

/// Bytes the queue holds without allocating. A frontend connection's whole
/// server preface — SETTINGS, the connection WINDOW_UPDATE and the ACK of the
/// client's SETTINGS, 79 bytes — fits, as do the handful of control frames a
/// pass usually queues, so a connection that never meets backpressure never
/// allocates for its output: the pool buffer `zero` used to carry it did not
/// either.
const INLINE_CAPACITY: usize = 128;

/// Capacity above which a drained queue gives its heap allocation back instead
/// of keeping it for the next spill. An adopted DATA rest can be a whole
/// frame, and holding that for the life of the connection would pay for every
/// past backpressure.
const RETAINED_CAPACITY: usize = 1024;

/// The ordered output queue. See the module documentation.
///
/// The bytes live in `inline[..end]` until they no longer fit, then move to
/// `heap` for as long as anything is queued (`spilled`).
#[derive(Debug)]
pub struct H2Output {
    inline: [u8; INLINE_CAPACITY],
    /// End of the queued bytes in `inline`; unused while `spilled`.
    end: usize,
    heap: Vec<u8>,
    spilled: bool,
    /// Index of the first unsent byte.
    head: usize,
    /// How many of the leading unsent bytes were already counted to the
    /// stream whose frame they finish.
    prepaid: usize,
}

impl Default for H2Output {
    fn default() -> Self {
        Self {
            inline: [0; INLINE_CAPACITY],
            end: 0,
            heap: Vec::new(),
            spilled: false,
            head: 0,
            prepaid: 0,
        }
    }
}

impl H2Output {
    /// The bytes still to send, oldest first.
    pub fn pending(&self) -> &[u8] {
        if self.spilled {
            &self.heap[self.head..]
        } else {
            &self.inline[self.head..self.end]
        }
    }

    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }

    pub fn len(&self) -> usize {
        self.stored() - self.head
    }

    fn stored(&self) -> usize {
        if self.spilled {
            self.heap.len()
        } else {
            self.end
        }
    }

    /// Make `room` bytes of space at the end and answer where they start.
    fn grow(&mut self, room: usize) -> usize {
        if !self.spilled {
            if self.end + room > INLINE_CAPACITY && self.head > 0 {
                self.inline.copy_within(self.head..self.end, 0);
                self.end -= self.head;
                self.head = 0;
            }
            if self.end + room <= INLINE_CAPACITY {
                let start = self.end;
                self.end += room;
                return start;
            }
            self.heap.clear();
            self.heap.reserve(self.end + room);
            self.heap.extend_from_slice(&self.inline[..self.end]);
            self.spilled = true;
        }
        let start = self.heap.len();
        self.heap.resize(start + room, 0);
        start
    }

    fn truncate(&mut self, stored: usize) {
        if self.spilled {
            self.heap.truncate(stored);
        } else {
            self.end = stored;
        }
    }

    fn storage_mut(&mut self) -> &mut [u8] {
        if self.spilled {
            &mut self.heap
        } else {
            &mut self.inline
        }
    }

    /// Queue whole frames written by `write` into `room` bytes of fresh
    /// space. `write` answers how many of them it used; on an error nothing
    /// is queued.
    pub fn push_frames<E>(
        &mut self,
        room: usize,
        write: impl FnOnce(&mut [u8]) -> Result<usize, E>,
    ) -> Result<usize, E> {
        let start = self.grow(room);
        match write(&mut self.storage_mut()[start..start + room]) {
            Ok(size) => {
                debug_assert!(size <= room, "a frame writer used more room than given");
                self.truncate(start + size.min(room));
                Ok(size)
            }
            Err(error) => {
                self.truncate(start);
                Err(error)
            }
        }
    }

    /// Queue one already serialised whole frame.
    pub fn push(&mut self, frame: &[u8]) {
        let start = self.grow(frame.len());
        self.storage_mut()[start..start + frame.len()].copy_from_slice(frame);
    }

    /// Account for `size` bytes the socket took from [`Self::pending`], and
    /// answer how many of them are connection overhead rather than the
    /// prepaid rest of a stream frame.
    pub fn consume(&mut self, size: usize) -> usize {
        debug_assert!(size <= self.len(), "consumed more output than queued");
        let size = size.min(self.len());
        self.head += size;
        let prepaid = size.min(self.prepaid);
        self.prepaid -= prepaid;
        if self.is_empty() {
            self.clear();
        } else {
            self.compact();
        }
        size - prepaid
    }

    /// Give back the heap bytes already sent while the queue is still in use.
    ///
    /// `consume` only advances `head`, and a queue under steady backpressure
    /// may never drain to empty, so without this the sent prefix would grow
    /// with every frame appended behind it. Once that prefix is both past
    /// [`RETAINED_CAPACITY`] and larger than what is still queued, the queued
    /// bytes move to the front (a copy no larger than the prefix it frees),
    /// and a capacity more than four times what is left is shrunk.
    fn compact(&mut self) {
        if !self.spilled || self.head < RETAINED_CAPACITY || self.head < self.len() {
            return;
        }
        self.heap.drain(..self.head);
        self.head = 0;
        let len = self.heap.len();
        if self.heap.capacity() > RETAINED_CAPACITY && self.heap.capacity() > 4 * len {
            self.heap.shrink_to(RETAINED_CAPACITY.max(2 * len));
        }
    }

    /// Drop everything queued: the peer is gone and nothing will be sent.
    /// Also where a drained queue returns to its inline storage.
    pub fn clear(&mut self) {
        self.head = 0;
        self.end = 0;
        self.prepaid = 0;
        self.spilled = false;
        if self.heap.capacity() > RETAINED_CAPACITY {
            self.heap = Vec::new();
        } else {
            self.heap.clear();
        }
    }

    /// Move the first `tail` bytes of `kawa.out` here and consume them from
    /// the stream: the unsent rest of the frame a partial write just cut, as
    /// measured by `h2_transmit::frame_tail` before the write was confirmed.
    ///
    /// The queue is empty when this runs, because a write only reaches a
    /// stream's bytes after every queued byte went first. The adopted bytes
    /// are prepaid: the caller counts them to the stream now.
    pub fn adopt_tail<T: AsBuffer>(&mut self, kawa: &mut Kawa<T>, tail: usize) {
        debug_assert!(
            self.is_empty(),
            "a stream frame rest can only be adopted by an empty output queue"
        );
        let buffer = kawa.storage.buffer();
        let mut left = tail;
        for block in kawa.out.iter() {
            if left == 0 {
                break;
            }
            match block {
                kawa::OutBlock::Delimiter => break,
                kawa::OutBlock::Store(store) => {
                    let data = store.data(buffer);
                    let size = data.len().min(left);
                    self.push(&data[..size]);
                    left -= size;
                }
            }
        }
        debug_assert_eq!(left, 0, "the frame rest must be queued in `kawa.out`");
        let adopted = tail - left;
        self.prepaid += adopted;
        kawa.consume(adopted);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use kawa::{Buffer, Kind, SliceBuffer, Store};

    #[test]
    fn frames_are_sent_in_the_order_they_were_queued() {
        let mut output = H2Output::default();
        output.push(b"first");
        let queued = output.push_frames(16, |buf| {
            buf[..6].copy_from_slice(b"second");
            Ok::<_, ()>(6)
        });
        assert_eq!(queued, Ok(6));
        assert_eq!(output.pending(), b"firstsecond");
        assert_eq!(output.consume(3), 3);
        assert_eq!(output.pending(), b"stsecond");
    }

    #[test]
    fn a_failed_frame_writer_queues_nothing() {
        let mut output = H2Output::default();
        output.push(b"kept");
        assert_eq!(
            output.push_frames(16, |_| Err::<usize, _>("full")),
            Err("full")
        );
        assert_eq!(output.pending(), b"kept");
    }

    #[test]
    fn an_adopted_tail_is_prepaid_and_control_bytes_behind_it_are_not() {
        let mut storage = [0u8; 64];
        let mut kawa = Kawa::new(Kind::Response, Buffer::new(SliceBuffer(&mut storage)));
        kawa.push_out(Store::Static(b"0123"));
        kawa.push_out(Store::Static(b"456789"));

        let mut output = H2Output::default();
        output.adopt_tail(&mut kawa, 7);
        output.push(b"ACK");

        assert_eq!(output.pending(), b"0123456ACK");
        assert_eq!(kawa.out.len(), 1, "the stream keeps what was not adopted");
        assert_eq!(output.consume(5), 0, "the adopted rest was counted already");
        assert_eq!(output.consume(5), 3, "only the ACK is overhead");
        assert!(output.is_empty());
    }

    #[test]
    fn a_drained_queue_gives_back_a_large_allocation() {
        let mut output = H2Output::default();
        output.push(&[0; RETAINED_CAPACITY + 1]);
        output.consume(RETAINED_CAPACITY + 1);
        assert_eq!(output.heap.capacity(), 0);
        assert!(!output.spilled);
    }

    #[test]
    fn a_server_preface_stays_inline() {
        let mut output = H2Output::default();
        output.push(&[1; 57]);
        output.push(&[2; 13]);
        output.push(&[3; 9]);
        assert!(!output.spilled, "79 bytes must not allocate");
        assert_eq!(output.heap.capacity(), 0);
        assert_eq!(output.len(), 79);
    }

    /// A queue that never drains to empty still gives its sent heap bytes
    /// back: under steady backpressure a frame is queued behind each partial
    /// send, and without compaction the sent prefix grew without bound.
    ///
    /// TO SEE THIS RED: in `H2Output::consume`, delete the `else` branch that
    /// calls `self.compact()`. The test then fails with `the heap must stay
    /// bounded while the queue never drains`. Verified 2026-09-27.
    #[test]
    fn a_queue_that_never_drains_stays_bounded() {
        let mut output = H2Output::default();
        let frame = [7u8; 4096];
        output.push(&frame);
        for _ in 0..1000 {
            output.push(&frame);
            assert_eq!(output.consume(frame.len()), frame.len());
            assert_eq!(output.len(), frame.len(), "one frame always left queued");
        }
        assert!(
            output.head <= RETAINED_CAPACITY.max(output.len())
                && output.heap.capacity() <= 4 * (RETAINED_CAPACITY + 2 * frame.len()),
            "the heap must stay bounded while the queue never drains: head {}, capacity {}",
            output.head,
            output.heap.capacity()
        );
        assert_eq!(output.pending(), &frame[..], "the queued frame is intact");
    }

    /// Crossing the inline capacity keeps every byte, in order: first by
    /// compacting what is left after a partial send, then by spilling to the
    /// heap, and back inline once drained.
    #[test]
    fn a_queue_past_its_inline_capacity_keeps_its_order() {
        let mut output = H2Output::default();
        let first: Vec<u8> = (0..100).collect();
        output.push(&first);
        assert_eq!(output.consume(60), 60);
        let second: Vec<u8> = (100..180).collect();
        output.push(&second);
        assert!(!output.spilled, "40 + 80 bytes fit once compacted");
        let third: Vec<u8> = (180..=255).collect();
        output.push(&third);
        assert!(output.spilled, "196 bytes do not fit inline");
        let expected: Vec<u8> = (60..=255).collect();
        assert_eq!(output.pending(), &expected[..]);
        assert_eq!(output.consume(expected.len()), expected.len());
        assert!(!output.spilled && output.is_empty());
        output.push(b"again");
        assert_eq!(output.pending(), b"again");
    }
}
