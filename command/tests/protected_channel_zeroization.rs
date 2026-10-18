//! The command channel never hands an allocator, a formatter or a snapshot
//! consumer a protected value it still holds.
//!
//! Three families of copy are covered: the growable buffer behind
//! `Channel::front_buf` / `Channel::back_buf` (consumed, shifted, grown,
//! shrunk and dropped storage), the encoded payload temporary of
//! `Channel::write_delimited_message`, and the bytes a `ChannelSnapshot` owns
//! (construction, clone, drop, serde growth and a failed deserialization).
//!
//! Allocations are observed with a global allocator that is bounded to the
//! blocks it handed out itself during one observation window on one thread.
//! Inside a window every allocation is served zero-initialised and registered,
//! so the oracle only ever reads bytes it initialised itself or that the code
//! under test wrote. When a registered block is released (`dealloc`, or a
//! `realloc`, which always moves here), the oracle looks for a run of
//! [`MIN_RUN`] consecutive marker bytes. Every protected byte used below is a
//! marker byte; nothing else in a window is, so a run means "a protected value
//! was released unwiped". All values are synthetic.
//!
//! The statics are process-wide, so every windowed test holds the guard returned
//! by [`observe`] for its whole body: two windows never overlap. In-place
//! erasure (a consumed prefix, a vacated tail) is read back from the live
//! allocation through a raw pointer, as the header-value test does.

use std::{
    alloc::{GlobalAlloc, Layout, System},
    cell::Cell,
    io::{Read, Write},
    ptr,
    sync::{
        Mutex, MutexGuard,
        atomic::{AtomicBool, AtomicUsize, Ordering},
    },
};

use prost::Message;
use serde::Serialize;
use sozu_command_lib::{
    buffer::growable::Buffer,
    channel::{Channel, ChannelError, ChannelSnapshot, PausedChannel, delimiter_size},
};

/// Registry capacity of the oracle. A window that needs more is a test bug.
const SLOTS: usize = 2048;
/// Consecutive marker bytes that make a released block "hold a protected value".
const MIN_RUN: usize = 8;

static BLOCK_PTR: [AtomicUsize; SLOTS] = [const { AtomicUsize::new(0) }; SLOTS];
static BLOCK_LEN: [AtomicUsize; SLOTS] = [const { AtomicUsize::new(0) }; SLOTS];
/// Registered blocks released since the window opened.
static RELEASES: AtomicUsize = AtomicUsize::new(0);
/// Released blocks that held a run of marker bytes.
static RESIDUE: AtomicUsize = AtomicUsize::new(0);
static OVERFLOW: AtomicBool = AtomicBool::new(false);

thread_local! {
    /// Only the thread that opened the window registers its allocations.
    static OBSERVING: Cell<bool> = const { Cell::new(false) };
}

fn observing() -> bool {
    OBSERVING.try_with(Cell::get).unwrap_or(false)
}

fn is_marker(byte: u8) -> bool {
    (0xE0..=0xEF).contains(&byte)
}

fn holds_marker(bytes: &[u8]) -> bool {
    let mut run = 0;
    for &byte in bytes {
        run = if is_marker(byte) { run + 1 } else { 0 };
        if run >= MIN_RUN {
            return true;
        }
    }
    false
}

struct Oracle;

impl Oracle {
    fn reset() {
        for (slot, len) in BLOCK_PTR.iter().zip(&BLOCK_LEN) {
            slot.store(0, Ordering::SeqCst);
            len.store(0, Ordering::SeqCst);
        }
        RELEASES.store(0, Ordering::SeqCst);
        RESIDUE.store(0, Ordering::SeqCst);
        OVERFLOW.store(false, Ordering::SeqCst);
    }

    fn register(ptr: *mut u8, len: usize) {
        for (slot, size) in BLOCK_PTR.iter().zip(&BLOCK_LEN) {
            if slot
                .compare_exchange(0, ptr as usize, Ordering::SeqCst, Ordering::SeqCst)
                .is_ok()
            {
                size.store(len, Ordering::SeqCst);
                return;
            }
        }
        OVERFLOW.store(true, Ordering::SeqCst);
    }

    fn is_tracked(ptr: *mut u8) -> bool {
        BLOCK_PTR
            .iter()
            .any(|slot| slot.load(Ordering::SeqCst) == ptr as usize)
    }

    /// Inspect and forget `ptr` if it is a registered block. Claiming the slot
    /// first makes the event happen once and stops tracking before the address
    /// can be handed out again.
    fn release(ptr: *mut u8) {
        for (slot, size) in BLOCK_PTR.iter().zip(&BLOCK_LEN) {
            if slot.load(Ordering::SeqCst) != ptr as usize {
                continue;
            }
            let len = size.load(Ordering::SeqCst);
            if slot
                .compare_exchange(ptr as usize, 0, Ordering::SeqCst, Ordering::SeqCst)
                .is_err()
            {
                return;
            }
            // SAFETY: registered blocks come from `System.alloc_zeroed` in
            // this allocator, so all `len` bytes are initialised, and the
            // block is still allocated: the caller releases it right after.
            let bytes = unsafe { std::slice::from_raw_parts(ptr, len) };
            RELEASES.fetch_add(1, Ordering::SeqCst);
            if holds_marker(bytes) {
                RESIDUE.fetch_add(1, Ordering::SeqCst);
            }
            return;
        }
    }
}

// SAFETY: every method forwards to `System` with the arguments it received.
// Inside an observation window the only additions are: serving the block
// zero-initialised, registering it, and reading it back (all of it initialised)
// right before it is released. `realloc` of a registered block always moves, so
// the old block is always released and inspected.
unsafe impl GlobalAlloc for Oracle {
    unsafe fn alloc(&self, layout: Layout) -> *mut u8 {
        if !observing() {
            return unsafe { System.alloc(layout) };
        }
        let block = unsafe { System.alloc_zeroed(layout) };
        if !block.is_null() {
            Oracle::register(block, layout.size());
        }
        block
    }

    unsafe fn alloc_zeroed(&self, layout: Layout) -> *mut u8 {
        let block = unsafe { System.alloc_zeroed(layout) };
        if observing() && !block.is_null() {
            Oracle::register(block, layout.size());
        }
        block
    }

    unsafe fn dealloc(&self, ptr: *mut u8, layout: Layout) {
        Oracle::release(ptr);
        unsafe { System.dealloc(ptr, layout) }
    }

    unsafe fn realloc(&self, ptr: *mut u8, old: Layout, size: usize) -> *mut u8 {
        if !Oracle::is_tracked(ptr) {
            return unsafe { System.realloc(ptr, old, size) };
        }
        let Ok(layout) = Layout::from_size_align(size, old.align()) else {
            return ptr::null_mut();
        };
        let new = unsafe { System.alloc_zeroed(layout) };
        if new.is_null() {
            return new;
        }
        unsafe { ptr::copy_nonoverlapping(ptr, new, old.size().min(size)) };
        Oracle::release(ptr);
        Oracle::register(new, size);
        unsafe { System.dealloc(ptr, old) };
        new
    }
}

#[global_allocator]
static ALLOCATOR: Oracle = Oracle;

static SERIAL: Mutex<()> = Mutex::new(());

/// An open observation window; closing it reports what was released.
struct Window {
    _serial: MutexGuard<'static, ()>,
}

struct Report {
    /// Registered blocks released inside the window.
    releases: usize,
    /// Of those, blocks released while holding a run of marker bytes.
    residue: usize,
}

/// Open a window on this thread. Allocate every protected value the code under
/// test must not be blamed for BEFORE calling this.
fn observe() -> Window {
    let serial = SERIAL
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner());
    Oracle::reset();
    OBSERVING.with(|observing| observing.set(true));
    Window { _serial: serial }
}

impl Window {
    fn close(self) -> Report {
        OBSERVING.with(|observing| observing.set(false));
        let report = Report {
            releases: RELEASES.load(Ordering::SeqCst),
            residue: RESIDUE.load(Ordering::SeqCst),
        };
        assert!(
            !OVERFLOW.load(Ordering::SeqCst),
            "the oracle registry overflowed"
        );
        report
    }
}

impl Drop for Window {
    fn drop(&mut self) {
        OBSERVING.with(|observing| observing.set(false));
        Oracle::reset();
    }
}

/// 64 bytes, every one a marker byte.
static MARK: [u8; 64] = marker();

const fn marker() -> [u8; 64] {
    let mut out = [0u8; 64];
    let mut index = 0;
    while index < out.len() {
        out[index] = 0xE0 + (index % 16) as u8;
        index += 1;
    }
    out
}

/// Copy `from..to` out of a live allocation starting at `base`.
fn peek(base: *const u8, from: usize, to: usize) -> Vec<u8> {
    let mut out = vec![0u8; to - from];
    // SAFETY: callers pass the start of an allocation that a `Buffer` still
    // owns, and a range inside its capacity; `Buffer` initialises every byte of
    // its backing storage (`vec![0; capacity]`), so the read is initialised.
    unsafe { ptr::copy_nonoverlapping(base.add(from), out.as_mut_ptr(), to - from) };
    out
}

fn assert_wiped(base: *const u8, from: usize, to: usize, what: &str) {
    assert!(
        peek(base, from, to).iter().all(|&byte| byte == 0),
        "{what}: bytes {from}..{to} were not wiped"
    );
}

/// A buffer of `capacity` holding the first `len` marker bytes.
fn filled(capacity: usize, len: usize) -> Buffer {
    let mut buffer = Buffer::with_capacity(capacity);
    buffer
        .write_all(&MARK[..len])
        .expect("the buffer has room for the marker");
    assert_eq!(buffer.available_data(), len);
    buffer
}

#[derive(Clone, PartialEq, prost::Message)]
struct Secret {
    #[prost(bytes = "vec", tag = "1")]
    value: Vec<u8>,
}

type Pair = (Channel<Secret, Secret>, Channel<Secret, Secret>);

fn channels(initial: u64, max: u64) -> Pair {
    Channel::generate_nonblocking(initial, max).expect("could not generate channels")
}

// ---- the oracle itself -----------------------------------------------------

#[test]
fn an_oracle_that_is_not_fooled_sees_an_unwiped_release() {
    let window = observe();
    let plain = MARK.to_vec();
    drop(plain);
    let mut grown = Vec::new();
    for byte in MARK {
        grown.push(byte);
    }
    drop(grown);
    let clean = vec![0u8; 64];
    drop(clean);
    let report = window.close();
    // Control: a plain Vec dropped (or regrown by `push`) is reported, a zero
    // one is not, so a pass elsewhere means the code wiped, not that the
    // oracle looks away.
    assert!(
        report.releases >= 3 && report.residue >= 2,
        "{} releases, {} with residue",
        report.releases,
        report.residue
    );
}

// ---- growable buffer: in-place erasure -------------------------------------

#[test]
fn consume_erases_the_bytes_it_retires() {
    let mut buffer = filled(64, 32);
    let base = buffer.data().as_ptr();
    assert_eq!(buffer.consume(8), 8);
    assert_eq!(buffer.data(), &MARK[8..32], "pending bytes are untouched");
    assert_eq!(peek(base, 8, 32), &MARK[8..32]);
    assert_wiped(base, 0, 8, "consume");
}

#[test]
fn read_erases_the_bytes_it_hands_out() {
    let mut buffer = filled(64, 32);
    let base = buffer.data().as_ptr();
    let mut out = [0u8; 8];
    assert_eq!(buffer.read(&mut out).expect("read"), 8);
    assert_eq!(out, MARK[..8], "the caller received the bytes");
    assert_eq!(buffer.data(), &MARK[8..32]);
    assert_wiped(base, 0, 8, "read");
}

#[test]
fn reset_erases_what_was_pending() {
    let mut buffer = filled(64, 32);
    let base = buffer.data().as_ptr();
    buffer.reset();
    assert!(buffer.empty());
    assert_eq!(buffer.available_space(), 64);
    assert_wiped(base, 0, 32, "reset");
}

#[test]
fn shift_erases_the_vacated_tail() {
    let mut buffer = filled(64, 40);
    let base = buffer.data().as_ptr();
    buffer.consume(10);
    buffer.shift();
    assert_eq!(buffer.data(), &MARK[10..40]);
    assert_eq!(peek(base, 0, 30), &MARK[10..40], "moved to the front");
    assert_wiped(base, 30, 40, "shift");

    // A consume past half the capacity compacts by itself.
    let mut buffer = filled(64, 40);
    let base = buffer.data().as_ptr();
    assert_eq!(buffer.consume(33), 33);
    assert_eq!(buffer.data(), &MARK[33..40]);
    assert_eq!(peek(base, 0, 7), &MARK[33..40]);
    assert_wiped(base, 7, 40, "compacting consume");
}

#[test]
fn a_refused_shrink_still_erases_what_its_compaction_vacated() {
    let mut buffer = filled(64, 40);
    let base = buffer.data().as_ptr();
    buffer.consume(8);
    assert!(!buffer.shrink(16), "32 pending bytes do not fit 16");
    assert_eq!(buffer.capacity(), 64);
    assert_eq!(buffer.data(), &MARK[8..40]);
    assert_wiped(base, 32, 40, "refused shrink");
}

#[test]
fn delete_slice_erases_the_bytes_it_closes_over() {
    let mut buffer = filled(64, 32);
    let base = buffer.data().as_ptr();
    assert_eq!(buffer.delete_slice(4, 8), Some(24));
    let mut expected = MARK[..4].to_vec();
    expected.extend_from_slice(&MARK[12..32]);
    assert_eq!(buffer.data(), expected.as_slice());
    assert_wiped(base, 24, 32, "delete_slice");
}

#[test]
fn replace_slice_that_shrinks_the_data_erases_the_vacated_tail() {
    let mut buffer = filled(64, 32);
    let base = buffer.data().as_ptr();
    assert_eq!(buffer.replace_slice(&[1, 2], 4, 8), Some(26));
    let mut expected = MARK[..4].to_vec();
    expected.extend_from_slice(&[1, 2]);
    expected.extend_from_slice(&MARK[12..32]);
    assert_eq!(buffer.data(), expected.as_slice());
    assert_wiped(base, 26, 32, "replace_slice");
}

// ---- growable buffer: allocation lifecycle ---------------------------------

#[test]
fn grow_erases_the_old_allocation_before_releasing_it() {
    let window = observe();
    let mut buffer = filled(32, 24);
    buffer.consume(4);
    let grew = buffer.grow(128);
    let report = window.close();

    assert!(grew);
    assert_eq!(buffer.capacity(), 128);
    assert_eq!(buffer.available_data(), 20);
    assert_eq!(
        buffer.available_space(),
        128 - 24,
        "the end offset, hence the backpressure arithmetic, is preserved"
    );
    assert_eq!(buffer.data(), &MARK[4..24]);
    assert_eq!(report.releases, 1, "the old allocation was released once");
    assert_eq!(report.residue, 0, "the old allocation was released unwiped");
}

#[test]
fn shrink_erases_the_old_allocation_before_releasing_it() {
    let window = observe();
    let mut buffer = filled(128, 24);
    buffer.consume(4);
    let shrank = buffer.shrink(32);
    let report = window.close();

    assert!(shrank);
    assert_eq!(buffer.capacity(), 32);
    assert_eq!(buffer.available_data(), 20);
    assert_eq!(buffer.available_space(), 12, "compacted to the front");
    assert_eq!(buffer.data(), &MARK[4..24]);
    assert_eq!(report.releases, 1, "the old allocation was released once");
    assert_eq!(report.residue, 0, "the old allocation was released unwiped");
}

#[test]
fn drop_and_clone_leave_no_protected_bytes_behind() {
    let window = observe();
    let buffer = filled(32, 24);
    let clone = buffer.clone();
    assert_eq!(clone.data(), buffer.data());
    drop(clone);
    drop(buffer);
    let report = window.close();
    assert_eq!(report.releases, 2, "the buffer and its clone");
    assert_eq!(report.residue, 0, "a dropped buffer was released unwiped");
}

#[test]
fn debug_reveals_no_buffer_bytes() {
    let buffer = filled(64, 32);
    for rendered in [format!("{buffer:?}"), format!("{buffer:#?}")] {
        let squeezed: String = rendered.split_whitespace().collect();
        assert!(!squeezed.contains("224,225,226,227"), "{rendered}");
        assert!(rendered.contains("Buffer"), "{rendered}");
    }
}

// ---- growable buffer: nothing observable changes ---------------------------

/// The pre-erasure algorithm, line for line, over a plain `Vec`.
struct Model {
    memory: Vec<u8>,
    capacity: usize,
    position: usize,
    end: usize,
}

impl Model {
    fn with_capacity(capacity: usize) -> Self {
        Model {
            memory: vec![0; capacity],
            capacity,
            position: 0,
            end: 0,
        }
    }
    fn available_data(&self) -> usize {
        self.end - self.position
    }
    fn available_space(&self) -> usize {
        self.capacity - self.end
    }
    fn data(&self) -> &[u8] {
        &self.memory[self.position..self.end]
    }
    fn grow(&mut self, size: usize) -> bool {
        if self.capacity >= size {
            return false;
        }
        self.memory.resize(size, 0);
        self.capacity = size;
        true
    }
    fn shift(&mut self) {
        if self.position > 0 {
            let length = self.end - self.position;
            self.memory.copy_within(self.position..self.end, 0);
            self.position = 0;
            self.end = length;
        }
    }
    fn shrink(&mut self, target: usize) -> bool {
        if target >= self.capacity {
            return false;
        }
        self.shift();
        if self.end > target {
            return false;
        }
        self.memory.truncate(target);
        self.memory.shrink_to_fit();
        self.capacity = target;
        true
    }
    fn consume(&mut self, count: usize) -> usize {
        let count = count.min(self.available_data());
        self.position += count;
        if self.position > self.capacity / 2 {
            self.shift();
        }
        count
    }
    fn fill(&mut self, count: usize) -> usize {
        let count = count.min(self.available_space());
        self.end += count;
        if self.available_space() < self.available_data() + count {
            self.shift();
        }
        count
    }
    fn write(&mut self, bytes: &[u8]) -> usize {
        let size = bytes.len().min(self.available_space());
        self.memory[self.end..self.end + size].copy_from_slice(&bytes[..size]);
        self.fill(size);
        size
    }
    fn read(&mut self, wanted: usize) -> Vec<u8> {
        let len = self.available_data().min(wanted);
        let out = self.memory[self.position..self.position + len].to_vec();
        self.position += len;
        out
    }
    fn reset(&mut self) {
        self.position = 0;
        self.end = 0;
    }
    fn delete_slice(&mut self, start: usize, length: usize) -> Option<usize> {
        if start + length >= self.available_data() {
            return None;
        }
        let begin = self.position + start;
        self.memory.copy_within(begin + length..self.end, begin);
        self.end -= length;
        Some(self.available_data())
    }
    fn replace_slice(&mut self, data: &[u8], start: usize, length: usize) -> Option<usize> {
        let data_len = data.len();
        if start + length > self.available_data()
            || self.position + start + data_len > self.capacity
        {
            return None;
        }
        let begin = self.position + start;
        if data_len < length {
            self.memory[begin..begin + data_len].copy_from_slice(data);
            self.memory
                .copy_within(start + length..self.end, begin + data_len);
            self.end -= length - data_len;
        } else {
            self.memory
                .copy_within(start + length..self.end, start + data_len);
            self.memory[begin..begin + data_len].copy_from_slice(data);
            self.end += data_len - length;
        }
        Some(self.available_data())
    }
    fn insert_slice(&mut self, data: &[u8], start: usize) -> Option<usize> {
        let data_len = data.len();
        if start > self.available_data() || self.position + self.end + data_len > self.capacity {
            return None;
        }
        let begin = self.position + start;
        self.memory.copy_within(start..self.end, start + data_len);
        self.memory[begin..begin + data_len].copy_from_slice(data);
        self.end += data_len;
        Some(self.available_data())
    }
}

#[test]
fn framing_capacity_and_backpressure_match_the_pre_erasure_algorithm() {
    let mut seed = 0x9E37_79B9_7F4A_7C15u64;
    let mut next = move |bound: usize| {
        seed ^= seed << 13;
        seed ^= seed >> 7;
        seed ^= seed << 17;
        (seed % bound as u64) as usize
    };
    let mut buffer = Buffer::with_capacity(16);
    let mut model = Model::with_capacity(16);
    for step in 0..6000 {
        let what = match next(11) {
            0 | 1 => {
                let len = next(40);
                let bytes = &MARK[..len];
                let got = buffer.write(bytes).expect("write");
                assert_eq!(got, model.write(bytes), "step {step} write");
                "write"
            }
            2 => {
                let count = next(40);
                assert_eq!(buffer.consume(count), model.consume(count), "step {step}");
                "consume"
            }
            3 => {
                buffer.shift();
                model.shift();
                "shift"
            }
            4 => {
                let size = next(160);
                assert_eq!(buffer.grow(size), model.grow(size), "step {step} grow");
                "grow"
            }
            5 => {
                let size = next(160);
                assert_eq!(
                    buffer.shrink(size),
                    model.shrink(size),
                    "step {step} shrink"
                );
                "shrink"
            }
            6 => {
                if next(8) == 0 {
                    buffer.reset();
                    model.reset();
                }
                "reset"
            }
            7 => {
                let wanted = next(40);
                let mut out = vec![0u8; wanted];
                let got = buffer.read(&mut out).expect("read");
                assert_eq!(&out[..got], model.read(wanted).as_slice(), "step {step}");
                "read"
            }
            8 => {
                let (start, length) = (next(40), next(12));
                assert_eq!(
                    buffer.delete_slice(start, length),
                    model.delete_slice(start, length),
                    "step {step} delete_slice"
                );
                "delete_slice"
            }
            9 => {
                // `replace_slice` and `insert_slice` index some ranges without
                // the buffer position, so they are only exercised where the
                // data is at the front, and only where the tail still fits.
                if model.position == 0 {
                    let (start, length, data_len) = (next(40), next(12), next(12));
                    if model.end + data_len <= model.capacity + length {
                        assert_eq!(
                            buffer.replace_slice(&MARK[..data_len], start, length),
                            model.replace_slice(&MARK[..data_len], start, length),
                            "step {step} replace_slice"
                        );
                    }
                }
                "replace_slice"
            }
            _ => {
                if model.position == 0 {
                    let (start, data_len) = (next(40), next(12));
                    assert_eq!(
                        buffer.insert_slice(&MARK[..data_len], start),
                        model.insert_slice(&MARK[..data_len], start),
                        "step {step} insert_slice"
                    );
                }
                "insert_slice"
            }
        };
        assert_eq!(buffer.capacity(), model.capacity, "step {step} {what}");
        assert_eq!(buffer.available_data(), model.available_data(), "{step}");
        assert_eq!(buffer.available_space(), model.available_space(), "{step}");
        assert_eq!(buffer.data(), model.data(), "step {step} {what}");
    }
}

// ---- encoded payload temporary ---------------------------------------------

#[test]
fn an_encoded_payload_is_wiped_when_the_frame_is_queued() {
    let secret = Secret {
        value: MARK.to_vec(),
    };
    let (mut channel, _peer) = channels(1000, 10000);
    let window = observe();
    let queued = channel.write_delimited_message(&secret);
    let report = window.close();

    queued.expect("the frame fits");
    assert_eq!(
        channel.back_buf.available_data(),
        delimiter_size() + secret.encoded_len(),
        "the frame is length-prefixed exactly as before"
    );
    assert!(report.releases >= 1, "the payload temporary was released");
    assert_eq!(
        report.residue, 0,
        "the encoded payload was released unwiped"
    );
}

#[test]
fn an_encoded_payload_is_wiped_when_the_frame_is_refused() {
    let secret = Secret {
        value: MARK.to_vec(),
    };
    let (mut channel, _peer) = channels(8, 64);
    let window = observe();
    let queued = channel.write_delimited_message(&secret);
    let report = window.close();

    assert!(
        matches!(queued, Err(ChannelError::MessageTooLarge { max: 64, .. })),
        "{queued:?}"
    );
    assert_eq!(channel.back_buf.available_data(), 0, "nothing was queued");
    assert!(report.releases >= 1, "the payload temporary was released");
    assert_eq!(
        report.residue, 0,
        "the encoded payload was released unwiped"
    );
}

// ---- snapshot --------------------------------------------------------------

/// A channel with `front` / `back` marker bytes pending in its buffers.
fn pending_channel(front: usize, back: usize) -> Pair {
    let (mut channel, peer) = channels(64, 128);
    channel
        .front_buf
        .write_all(&MARK[..front])
        .expect("front room");
    channel
        .back_buf
        .write_all(&MARK[..back])
        .expect("back room");
    (channel, peer)
}

#[test]
fn a_snapshot_leaves_no_protected_bytes_when_cloned_and_dropped() {
    let (channel, _peer) = pending_channel(40, 24);
    let window = observe();
    let snapshot = channel.snapshot();
    let clone = snapshot.clone();
    assert_eq!(clone, snapshot);
    assert_eq!(snapshot.buffered_bytes(), 64);
    drop(clone);
    drop(snapshot);
    let report = window.close();
    assert_eq!(report.releases, 4, "front and back, snapshot and clone");
    assert_eq!(report.residue, 0, "a snapshot was released unwiped");
}

#[test]
fn restoring_a_snapshot_leaves_no_superseded_copy_behind() {
    let (mut channel, _peer) = channels(8, 64);
    channel.front_buf.grow(32);
    channel
        .front_buf
        .write_all(&MARK[..20])
        .expect("front room");
    channel.back_buf.grow(48);
    channel.back_buf.write_all(&MARK[..24]).expect("back room");

    let window = observe();
    let snapshot = channel.snapshot();
    let paused: PausedChannel<Secret, Secret> =
        Channel::restore_paused(channel.sock, snapshot, 8, 64).expect("valid snapshot");
    let restored = paused.resume();
    let same_front = restored.front_buf.data() == &MARK[..20];
    let same_back = restored.back_buf.data() == &MARK[..24];
    let capacities = (restored.front_buf.capacity(), restored.back_buf.capacity());
    drop(restored);
    let report = window.close();

    assert!(same_front && same_back, "the pending bytes round-trip");
    assert_eq!(capacities, (32, 48), "capacities round-trip");
    assert!(report.releases >= 4, "{} releases", report.releases);
    assert_eq!(report.residue, 0, "a superseded copy was released unwiped");
}

#[test]
fn snapshot_debug_reveals_no_bytes() {
    let (channel, _peer) = pending_channel(40, 24);
    let snapshot = channel.snapshot();
    for rendered in [format!("{snapshot:?}"), format!("{snapshot:#?}")] {
        let squeezed: String = rendered.split_whitespace().collect();
        assert!(!squeezed.contains("224,225,226,227"), "{rendered}");
        assert!(rendered.contains("ChannelSnapshot"), "{rendered}");
    }
}

/// The JSON of a real, empty-buffer snapshot, ready to splice arrays into.
fn json_template() -> String {
    let (channel, _peer) = channels(8, 64);
    let json = serde_json::to_string(&channel.snapshot()).expect("serialize");
    assert!(json.contains(r#""front":[],"back":[],"#), "{json}");
    json
}

fn items(bytes: &[u8]) -> String {
    let items: Vec<String> = bytes.iter().map(ToString::to_string).collect();
    items.join(",")
}

fn array(bytes: &[u8]) -> String {
    format!("[{}]", items(bytes))
}

fn splice(front: &str, back: &str) -> String {
    json_template().replace(
        r#""front":[],"back":[],"#,
        &format!(r#""front":{front},"back":{back},"#),
    )
}

#[test]
fn a_failed_deserialization_leaves_no_partial_bytes_behind() {
    // 20 accepted marker elements, then one a `u8` cannot hold.
    let mid_front = splice(&format!("[{},300]", items(&MARK[..20])), "[]");
    let mid_back = splice(&array(&MARK[..24]), &format!("[{},-1]", items(&MARK[..20])));
    let late_field = splice(&array(&MARK[..40]), &array(&MARK[..24]))
        .replace(r#""blocking":false"#, r#""blocking":"no""#);
    let missing_field = splice(&array(&MARK[..40]), &array(&MARK[..24]))
        .replace(r#""version":1,"#, "")
        .replace(r#""interest":1,"#, "");
    let wrong_shape = splice(&array(&MARK[..40]), "\"not a sequence\"");

    for (what, json) in [
        ("an element out of range in the middle of front", mid_front),
        ("an element out of range in the middle of back", mid_back),
        ("a later field of the wrong type", late_field),
        ("absent fields", missing_field),
        ("a back that is not a sequence", wrong_shape),
    ] {
        let window = observe();
        let outcome = serde_json::from_str::<ChannelSnapshot>(&json);
        let failed = outcome.is_err();
        drop(outcome);
        let report = window.close();
        assert!(failed, "{what}: the input was meant to be refused");
        assert!(report.releases >= 1, "{what}: nothing was received");
        assert_eq!(
            report.residue, 0,
            "{what}: a partial value was released unwiped"
        );
    }
}

#[test]
fn deserialization_growth_leaves_no_superseded_allocation_behind() {
    let front: Vec<u8> = (0..200).map(|index| MARK[index % 64]).collect();
    let back: Vec<u8> = (0..100).map(|index| MARK[index % 64]).collect();
    let json = splice(&array(&front), &array(&back));

    let window = observe();
    let snapshot: ChannelSnapshot = serde_json::from_str(&json).expect("a legacy snapshot");
    let buffered = snapshot.buffered_bytes();
    drop(snapshot);
    let report = window.close();

    assert_eq!(buffered, 300);
    assert!(report.releases >= 2, "{} releases", report.releases);
    assert_eq!(report.residue, 0, "a grown or dropped array kept the bytes");
}

/// Serialises exactly like a snapshot whose byte fields are plain `Vec<u8>`.
#[derive(Serialize)]
struct LegacySnapshot {
    version: u16,
    front: Vec<u8>,
    back: Vec<u8>,
    initial_buffer_size: usize,
    max_buffer_size: usize,
    front_buffer_capacity: usize,
    back_buffer_capacity: usize,
    readiness: u16,
    interest: u16,
    blocking: bool,
    front_high_watermark_logged: bool,
    back_high_watermark_logged: bool,
}

#[test]
fn the_wire_json_and_legacy_partial_frames_are_unchanged() {
    let (mut channel, _peer) = channels(8, 64);
    channel.front_buf.grow(32);
    // A cut length prefix and a cut payload: partial frames must survive.
    channel.front_buf.write_all(&[1, 2, 3]).expect("front room");
    channel
        .back_buf
        .write_all(&[9, 8, 7, 6])
        .expect("back room");
    let (readiness, interest) = (channel.readiness.0, channel.interest.0);

    let json = serde_json::to_string(&channel.snapshot()).expect("serialize");
    let expected = format!(
        r#"{{"version":1,"front":[1,2,3],"back":[9,8,7,6],"initial_buffer_size":8,"max_buffer_size":64,"front_buffer_capacity":32,"back_buffer_capacity":8,"readiness":{readiness},"interest":{interest},"blocking":false,"front_high_watermark_logged":false,"back_high_watermark_logged":false}}"#
    );
    assert_eq!(json, expected);
    let legacy = LegacySnapshot {
        version: 1,
        front: vec![1, 2, 3],
        back: vec![9, 8, 7, 6],
        initial_buffer_size: 8,
        max_buffer_size: 64,
        front_buffer_capacity: 32,
        back_buffer_capacity: 8,
        readiness,
        interest,
        blocking: false,
        front_high_watermark_logged: false,
        back_high_watermark_logged: false,
    };
    assert_eq!(
        serde_json::to_string(&legacy).expect("serialize"),
        json,
        "byte-identical to a plain Vec<u8> snapshot"
    );

    let decoded: ChannelSnapshot = serde_json::from_str(&json).expect("deserialize");
    assert!(decoded.has_buffered_input() && decoded.has_buffered_output());
    assert_eq!(decoded.buffered_bytes(), 7);
    assert_eq!(serde_json::to_string(&decoded).expect("serialize"), json);

    let paused: PausedChannel<Secret, Secret> =
        Channel::restore_paused(channel.sock, decoded, 8, 64).expect("valid snapshot");
    let restored = paused.resume();
    assert_eq!(restored.front_buf.data(), [1, 2, 3]);
    assert_eq!(restored.back_buf.data(), [9, 8, 7, 6]);
    assert_eq!(restored.front_buf.capacity(), 32);
    assert_eq!(restored.back_buf.capacity(), 8);

    // Values serde has always refused are still refused.
    for refused in [
        json.replace("[1,2,3]", "[1,256]"),
        json.replace("[1,2,3]", "\"abc\""),
        json.replace("[1,2,3]", "null"),
        json.replace("[1,2,3]", "{\"0\":1}"),
    ] {
        assert!(serde_json::from_str::<ChannelSnapshot>(&refused).is_err());
    }
}
