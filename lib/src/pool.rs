/// experimental module to measure buffer pool usage
///
/// this allows us to track how many buffers are used through the
/// buffers.count metric.
///
/// Right now, we wrap the `pool` crate, but we might write a different
/// buffer pool in the future, so this module will still be useful to
/// test the differences
use std::{
    cmp,
    io::{self, Read, Write},
    ops, ptr,
    sync::atomic::{AtomicUsize, Ordering},
};

use crate::metrics::names;

static BUFFER_COUNT: AtomicUsize = AtomicUsize::new(0);

pub struct Pool {
    pub inner: poule::Pool<BufferMetadata>,
    pub buffer_size: usize,
}

impl Pool {
    pub fn with_capacity(minimum: usize, maximum: usize, buffer_size: usize) -> Pool {
        debug_assert!(
            minimum <= maximum,
            "pool minimum ({minimum}) must not exceed maximum ({maximum})"
        );
        let mut inner = poule::Pool::with_extra(maximum, buffer_size);
        inner.grow_to(minimum);
        let pool = Pool { inner, buffer_size };
        // Post-condition: a fresh pool hands out nothing and respects its
        // capacity ceiling. `grow_to(minimum)` may pre-allocate, but never
        // beyond `maximum`, and nothing is checked out yet.
        debug_assert_eq!(pool.inner.used(), 0, "a fresh pool has nothing checked out");
        debug_assert!(
            pool.inner.capacity() <= maximum,
            "grown capacity must never exceed the configured maximum"
        );
        #[cfg(debug_assertions)]
        pool.check_invariants();
        pool
    }

    pub fn checkout(&mut self) -> Option<Checkout> {
        // Pre-condition: the accounting invariant holds on entry.
        #[cfg(debug_assertions)]
        self.check_invariants();
        // Snapshot used-count before any growth or checkout so the
        // post-conditions can assert the exact delta. Read only inside
        // `debug_assert!` → dead code in release, but it must still compile.
        let used_before = self.inner.used();

        if self.inner.used() == self.inner.capacity()
            && self.inner.capacity() < self.inner.maximum_capacity()
        {
            self.inner.grow_to(std::cmp::min(
                self.inner.capacity() * 2,
                self.inner.maximum_capacity(),
            ));
            debug!(
                "growing pool capacity from {} to {}",
                self.inner.capacity(),
                std::cmp::min(self.inner.capacity() * 2, self.inner.maximum_capacity())
            );
        }
        let capacity = self.buffer_size;
        let buffer_size = self.buffer_size;
        let result = self
            .inner
            .checkout(|| {
                trace!("initializing a buffer with capacity {}", capacity);
                BufferMetadata::new()
            })
            .map(|c| {
                let old_buffer_count = BUFFER_COUNT.fetch_add(1, Ordering::SeqCst);
                gauge!(names::buffer::IN_USE, old_buffer_count + 1);
                Checkout { inner: c }
            });

        match &result {
            Some(checkout) => {
                // A successful checkout consumes exactly one slot: the pool's
                // used-count rose by one and never exceeded the live capacity.
                debug_assert_eq!(
                    self.inner.used(),
                    used_before + 1,
                    "a successful checkout must increment used-count by exactly 1"
                );
                debug_assert!(
                    self.inner.used() <= self.inner.capacity(),
                    "used-count must never exceed the pool capacity"
                );
                // The handed-out buffer must carry at least the configured size
                // (poule rounds the per-entry extra up to `align_of::<Entry>`, so
                // capacity equals buffer_size only when it is already aligned —
                // the default 16393 rounds to 16400) and start empty
                // (position == end == 0 from `BufferMetadata::new`).
                debug_assert!(
                    checkout.capacity() >= buffer_size,
                    "a checked-out buffer must hold at least the configured buffer size"
                );
                debug_assert_eq!(
                    checkout.available_data(),
                    0,
                    "a freshly checked-out buffer must hold no data"
                );
            }
            None => {
                // Exhaustion is graceful (never a panic): the used-count must
                // be unchanged on the failure path. The pool was at its hard
                // ceiling, so capacity equals maximum_capacity.
                debug_assert_eq!(
                    self.inner.used(),
                    used_before,
                    "a failed checkout must not change the used-count"
                );
                debug_assert_eq!(
                    self.inner.capacity(),
                    self.inner.maximum_capacity(),
                    "checkout only fails once the pool is grown to its maximum"
                );
            }
        }

        // Post-condition: the accounting invariant still holds on exit.
        #[cfg(debug_assertions)]
        self.check_invariants();
        result
    }

    /// Full accounting-invariant sweep for the buffer pool, used as a
    /// `debug_assert!`-guarded pre/post-condition on every public mutating
    /// method. Encodes the `available + checked_out == capacity` contract in
    /// terms of `poule`'s accounting: the number of checked-out buffers
    /// (`used`) plus the number still available equals the live capacity, and
    /// capacity stays bounded by the configured hard maximum. Compiled out
    /// entirely in release.
    #[cfg(debug_assertions)]
    fn check_invariants(&self) {
        let used = self.inner.used();
        let capacity = self.inner.capacity();
        let maximum = self.inner.maximum_capacity();

        // `available + checked_out == capacity`: every slot in the live
        // capacity is either checked out or available, never both, never lost.
        debug_assert!(
            used <= capacity,
            "checked-out buffers ({used}) must never exceed live capacity ({capacity})"
        );
        let available = capacity - used;
        debug_assert_eq!(
            available + used,
            capacity,
            "available ({available}) + checked_out ({used}) must equal capacity ({capacity})"
        );

        // Capacity grows lazily but is hard-bounded by the configured maximum.
        debug_assert!(
            capacity <= maximum,
            "live capacity ({capacity}) must never exceed maximum_capacity ({maximum})"
        );
    }
}

impl ops::Deref for Pool {
    type Target = poule::Pool<BufferMetadata>;

    fn deref(&self) -> &Self::Target {
        &self.inner
    }
}

impl ops::DerefMut for Pool {
    fn deref_mut(&mut self) -> &mut poule::Pool<BufferMetadata> {
        &mut self.inner
    }
}

#[derive(Debug, PartialEq, Eq, Clone)]
pub struct BufferMetadata {
    position: usize,
    end: usize,
}

impl Default for BufferMetadata {
    fn default() -> Self {
        Self::new()
    }
}

impl BufferMetadata {
    pub fn new() -> BufferMetadata {
        BufferMetadata {
            position: 0,
            end: 0,
        }
    }
}

pub struct Checkout {
    pub inner: poule::Checkout<BufferMetadata>,
}

/*
impl ops::Deref for Checkout {
    type Target = poule::Checkout<BufferMetadata>;

    fn deref(&self) -> &Self::Target {
        &self.inner
    }
}

impl ops::DerefMut for Checkout {
    fn deref_mut(&mut self) -> &mut poule::Checkout<BufferMetadata> {
        &mut self.inner
    }
}
*/

impl Drop for Checkout {
    fn drop(&mut self) {
        let old_buffer_count = BUFFER_COUNT.fetch_sub(1, Ordering::SeqCst);
        // Gauge-underflow guard: every live `Checkout` was paired with a
        // `fetch_add(1)` at checkout time, so the global in-use counter must
        // be strictly positive when one is dropped. A zero here means a
        // double-checkin or an unbalanced add/sub — a real accounting bug, not
        // a rounding issue. `fetch_sub` wraps in release; the assert makes the
        // violation loud in debug/test/fuzz.
        debug_assert!(
            old_buffer_count >= 1,
            "buffer in-use gauge underflow on checkin: count was {old_buffer_count} before decrement"
        );
        gauge!(names::buffer::IN_USE, old_buffer_count - 1);
    }
}

impl Checkout {
    pub fn available_data(&self) -> usize {
        self.inner.end - self.inner.position
    }

    pub fn available_space(&self) -> usize {
        self.capacity() - self.inner.end
    }

    pub fn capacity(&self) -> usize {
        self.inner.extra().len()
    }

    pub fn empty(&self) -> bool {
        self.inner.position == self.inner.end
    }

    /// Internal buffer-window invariant: `position <= end <= capacity`. The
    /// occupied window `[position, end)` is the live data; everything outside
    /// it is free space. Used as a `debug_assert!`-guarded pre/post-condition
    /// on the slice-mutating methods. Compiled out in release.
    #[cfg(debug_assertions)]
    fn check_invariants(&self) {
        let position = self.inner.position;
        let end = self.inner.end;
        let capacity = self.capacity();
        debug_assert!(
            position <= end,
            "buffer position ({position}) must not pass end ({end})"
        );
        debug_assert!(
            end <= capacity,
            "buffer end ({end}) must not exceed capacity ({capacity})"
        );
        // consumed-prefix + available_data + available_space == capacity:
        // the three regions partition the buffer with no overlap or loss.
        debug_assert_eq!(
            position + self.available_data() + self.available_space(),
            capacity,
            "consumed prefix + available data + free space must tile the whole buffer"
        );
    }

    pub fn consume(&mut self, count: usize) -> usize {
        #[cfg(debug_assertions)]
        self.check_invariants();
        let available_before = self.available_data();
        let cnt = cmp::min(count, available_before);
        // `consume` can never advance past the available data — `cnt` is
        // clamped above, so this read can never underflow `available_data`.
        debug_assert!(
            cnt <= available_before,
            "consume count ({cnt}) must not exceed available data ({available_before})"
        );
        self.inner.position += cnt;
        if self.inner.position > self.capacity() / 2 {
            //trace!("consume shift: pos {}, end {}", self.position, self.end);
            self.shift();
        }
        // Post-condition: exactly `cnt` bytes left the readable window (a
        // shift relocates but does not change `available_data`).
        debug_assert_eq!(
            self.available_data(),
            available_before - cnt,
            "consume must shrink available data by exactly the consumed count"
        );
        #[cfg(debug_assertions)]
        self.check_invariants();
        cnt
    }

    pub fn fill(&mut self, count: usize) -> usize {
        #[cfg(debug_assertions)]
        self.check_invariants();
        let data_before = self.available_data();
        let space_before = self.available_space();
        let cnt = cmp::min(count, space_before);
        // `fill` can never claim more than the free space — `cnt` is clamped,
        // so advancing `end` by `cnt` can never overrun the buffer.
        debug_assert!(
            cnt <= space_before,
            "fill count ({cnt}) must not exceed available space ({space_before})"
        );
        self.inner.end += cnt;
        if self.available_space() < self.available_data() + cnt {
            //trace!("fill shift: pos {}, end {}", self.position, self.end);
            self.shift();
        }
        // Post-condition: exactly `cnt` bytes entered the readable window (a
        // shift relocates but does not change `available_data`).
        debug_assert_eq!(
            self.available_data(),
            data_before + cnt,
            "fill must grow available data by exactly the filled count"
        );
        #[cfg(debug_assertions)]
        self.check_invariants();
        cnt
    }

    pub fn reset(&mut self) {
        self.inner.position = 0;
        self.inner.end = 0;
        // Post-condition: a reset buffer is empty and offers its full
        // capacity as free space.
        debug_assert_eq!(self.available_data(), 0, "reset must empty the buffer");
        debug_assert_eq!(
            self.available_space(),
            self.capacity(),
            "reset must restore the full capacity as free space"
        );
        #[cfg(debug_assertions)]
        self.check_invariants();
    }

    pub fn sync(&mut self, end: usize, position: usize) {
        // Pre-condition: the caller must supply a coherent window —
        // `position <= end <= capacity`. `sync` restores a previously valid
        // window (e.g. after a `Vec`-backed parse), so a violation here is a
        // caller logic bug, not network input.
        debug_assert!(
            position <= end,
            "sync position ({position}) must not pass end ({end})"
        );
        debug_assert!(
            end <= self.capacity(),
            "sync end ({end}) must not exceed capacity ({})",
            self.capacity()
        );
        self.inner.position = position;
        self.inner.end = end;
        // Post-condition: the readable window matches the requested span.
        debug_assert_eq!(
            self.available_data(),
            end - position,
            "sync must expose exactly end - position readable bytes"
        );
        #[cfg(debug_assertions)]
        self.check_invariants();
    }

    pub fn data(&self) -> &[u8] {
        &self.inner.extra()[self.inner.position..self.inner.end]
    }

    pub fn space(&mut self) -> &mut [u8] {
        let range = self.inner.end..self.capacity();
        &mut self.inner.extra_mut()[range]
    }

    pub fn shift(&mut self) {
        #[cfg(debug_assertions)]
        self.check_invariants();
        let pos = self.inner.position;
        let end = self.inner.end;
        let data_before = self.available_data();
        if pos > 0 {
            // SAFETY: src and dst point into the same checkout buffer
            // (`self.inner.extra`); the slice indexing above bounds-checks
            // both ranges (`pos..end` and `..length`) against the live
            // buffer length. `ptr::copy` is overlap-safe.
            unsafe {
                let length = end - pos;
                ptr::copy(
                    self.inner.extra()[pos..end].as_ptr(),
                    self.inner.extra_mut()[..length].as_mut_ptr(),
                    length,
                );
                self.inner.position = 0;
                self.inner.end = length;
            }
        }
        // Post-condition: shift relocates the readable window to the front but
        // never changes how many bytes are readable.
        debug_assert_eq!(
            self.available_data(),
            data_before,
            "shift must preserve the amount of readable data"
        );
        #[cfg(debug_assertions)]
        self.check_invariants();
    }

    pub fn delete_slice(&mut self, start: usize, length: usize) -> Option<usize> {
        #[cfg(debug_assertions)]
        self.check_invariants();
        let data_before = self.available_data();
        if start + length >= self.available_data() {
            // Out-of-range deletes are a graceful no-op: nothing changed.
            debug_assert_eq!(
                self.available_data(),
                data_before,
                "rejected delete_slice must not mutate the buffer"
            );
            return None;
        }

        // SAFETY: src and dst point into the same checkout buffer
        // (`self.inner.extra`). The early-return above guarantees
        // `start + length < available_data`, and slice indexing
        // bounds-checks both `begin+length..end` and `begin..next_end`
        // against the live buffer length. `ptr::copy` is overlap-safe.
        unsafe {
            let begin = self.inner.position + start;
            let next_end = self.inner.end - length;
            ptr::copy(
                self.inner.extra()[begin + length..self.inner.end].as_ptr(),
                self.inner.extra_mut()[begin..next_end].as_mut_ptr(),
                self.inner.end - (begin + length),
            );
            self.inner.end = next_end;
        }
        // Post-condition: removing `length` bytes from the middle shrinks the
        // readable window by exactly `length`.
        debug_assert_eq!(
            self.available_data(),
            data_before - length,
            "delete_slice must shrink available data by exactly the deleted length"
        );
        #[cfg(debug_assertions)]
        self.check_invariants();
        Some(self.available_data())
    }

    pub fn replace_slice(&mut self, data: &[u8], start: usize, length: usize) -> Option<usize> {
        #[cfg(debug_assertions)]
        self.check_invariants();
        let data_before = self.available_data();
        let data_len = data.len();
        if start + length > self.available_data()
            || self.inner.position + start + data_len > self.capacity()
        {
            // Rejected replace is a graceful no-op.
            debug_assert_eq!(
                self.available_data(),
                data_before,
                "rejected replace_slice must not mutate the buffer"
            );
            return None;
        }
        // Pre-condition: the replacement window lies within the readable data
        // (guaranteed by the early-return above). The net length change is
        // `data_len - length`.
        debug_assert!(
            start + length <= data_before,
            "replace_slice window [{start}, {}) must lie within available data ({data_before})",
            start + length
        );

        // SAFETY: every `ptr::copy` below moves bytes inside the same
        // checkout buffer (`self.inner.extra`) or copies from the caller's
        // `data` slice into it. The two early-return checks above bound
        // the affected ranges against `available_data()` and `capacity()`,
        // and each slice indexing site is bounds-checked. `ptr::copy` is
        // overlap-safe.
        unsafe {
            let begin = self.inner.position + start;
            let slice_end = begin + data_len;
            // we reduced the data size
            if data_len < length {
                ptr::copy(
                    data.as_ptr(),
                    self.inner.extra_mut()[begin..slice_end].as_mut_ptr(),
                    data_len,
                );

                ptr::copy(
                    self.inner.extra()[start + length..self.inner.end].as_ptr(),
                    self.inner.extra_mut()[slice_end..].as_mut_ptr(),
                    self.inner.end - (start + length),
                );
                self.inner.end -= length - data_len;

            // we put more data in the buffer
            } else {
                ptr::copy(
                    self.inner.extra()[start + length..self.inner.end].as_ptr(),
                    self.inner.extra_mut()[start + data_len..].as_mut_ptr(),
                    self.inner.end - (start + length),
                );
                ptr::copy(
                    data.as_ptr(),
                    self.inner.extra_mut()[begin..slice_end].as_mut_ptr(),
                    data_len,
                );
                self.inner.end += data_len - length;
            }
        }
        // Post-condition: the readable window grew/shrank by exactly the net
        // difference between the inserted data and the replaced span. Compare
        // as i64 to keep the arithmetic signed and avoid usize underflow in
        // the assertion itself.
        debug_assert_eq!(
            self.available_data() as i64,
            data_before as i64 + data_len as i64 - length as i64,
            "replace_slice must change available data by exactly data_len - length"
        );
        #[cfg(debug_assertions)]
        self.check_invariants();
        Some(self.available_data())
    }

    pub fn insert_slice(&mut self, data: &[u8], start: usize) -> Option<usize> {
        #[cfg(debug_assertions)]
        self.check_invariants();
        let data_before = self.available_data();
        let data_len = data.len();
        if start > self.available_data()
            || self.inner.position + self.inner.end + data_len > self.capacity()
        {
            // Rejected insert is a graceful no-op.
            debug_assert_eq!(
                self.available_data(),
                data_before,
                "rejected insert_slice must not mutate the buffer"
            );
            return None;
        }
        // Pre-condition: the insertion point lies within the readable data and
        // the resulting buffer stays within capacity (guaranteed above).
        debug_assert!(
            start <= data_before,
            "insert_slice start ({start}) must lie within available data ({data_before})"
        );

        // SAFETY: both `ptr::copy` calls touch `self.inner.extra` (same
        // allocation) or copy from `data` into it. The early-return checks
        // bound `start <= available_data` and the resulting tail
        // `position + end + data_len <= capacity`, and each slice indexing
        // site is bounds-checked. `ptr::copy` is overlap-safe.
        unsafe {
            let begin = self.inner.position + start;
            let slice_end = begin + data_len;
            ptr::copy(
                self.inner.extra()[start..self.inner.end].as_ptr(),
                self.inner.extra_mut()[start + data_len..].as_mut_ptr(),
                self.inner.end - start,
            );
            ptr::copy(
                data.as_ptr(),
                self.inner.extra_mut()[begin..slice_end].as_mut_ptr(),
                data_len,
            );
            self.inner.end += data_len;
        }
        // Post-condition: inserting `data_len` bytes grows the readable window
        // by exactly `data_len`.
        debug_assert_eq!(
            self.available_data(),
            data_before + data_len,
            "insert_slice must grow available data by exactly the inserted length"
        );
        #[cfg(debug_assertions)]
        self.check_invariants();
        Some(self.available_data())
    }
}

impl Write for Checkout {
    fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
        match self.space().write(buf) {
            Ok(size) => {
                self.fill(size);
                Ok(size)
            }
            err => err,
        }
    }

    fn flush(&mut self) -> io::Result<()> {
        Ok(())
    }
}

impl Read for Checkout {
    fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
        let len = cmp::min(self.available_data(), buf.len());
        // SAFETY: `len = min(available_data, buf.len())`, so the source
        // range `position..position+len` lies inside `self.inner.extra` and
        // the destination `buf[..len]` fits the caller's `&mut [u8]`. The
        // two slices are in different allocations; `ptr::copy` is
        // overlap-safe regardless.
        unsafe {
            ptr::copy(
                self.inner.extra()[self.inner.position..self.inner.position + len].as_ptr(),
                buf.as_mut_ptr(),
                len,
            );
            self.inner.position += len;
        }
        Ok(len)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::{Read, Write};

    /// Helper: create a Pool and return it directly.
    fn create_test_pool(buffer_size: usize, max_count: usize) -> Pool {
        Pool::with_capacity(max_count, max_count, buffer_size)
    }

    /// Helper: checkout a buffer and write initial content into it.
    fn checkout_with_data(pool: &mut Pool, data: &[u8]) -> Checkout {
        let mut buf = pool.checkout().expect("checkout should succeed");
        let n = buf.write(data).expect("write should succeed");
        assert_eq!(n, data.len(), "all bytes should be written");
        buf
    }

    // -----------------------------------------------------------------------
    // Pool checkout / checkin lifecycle
    // -----------------------------------------------------------------------

    #[test]
    fn test_pool_checkout_returns_buffer() {
        let mut pool = create_test_pool(1024, 2);
        let buf = pool.checkout();
        assert!(
            buf.is_some(),
            "first checkout from a fresh pool must succeed"
        );
        let buf = buf.unwrap();
        assert_eq!(buf.capacity(), 1024);
        assert_eq!(buf.available_data(), 0);
        assert_eq!(buf.available_space(), 1024);
    }

    #[test]
    fn test_pool_checkin_on_drop() {
        let mut pool = create_test_pool(128, 1);
        {
            let _buf = pool.checkout().expect("checkout should succeed");
            assert_eq!(pool.inner.used(), 1);
        }
        assert_eq!(pool.inner.used(), 0);
        let buf2 = pool.checkout();
        assert!(buf2.is_some(), "checkout after checkin should succeed");
    }

    #[test]
    fn test_pool_auto_grow() {
        let mut pool = Pool::with_capacity(1, 4, 256);
        let _b1 = pool.checkout().expect("first checkout");
        let _b2 = pool.checkout().expect("second checkout triggers growth");
        let _b3 = pool.checkout().expect("third checkout");
    }

    // -----------------------------------------------------------------------
    // Write / Read trait impls
    // -----------------------------------------------------------------------

    #[test]
    fn test_checkout_write_and_read_data() {
        let mut pool = create_test_pool(1024, 2);
        let mut buf = pool.checkout().unwrap();

        let payload = b"hello world";
        let written = buf.write(payload).unwrap();
        assert_eq!(written, payload.len());
        assert_eq!(buf.available_data(), payload.len());
        assert_eq!(buf.data(), payload);
    }

    #[test]
    fn test_checkout_read_trait() {
        let mut pool = create_test_pool(1024, 2);
        let mut buf = checkout_with_data(&mut pool, b"hello");

        let mut out = [0u8; 5];
        let n = buf.read(&mut out).unwrap();
        assert_eq!(n, 5);
        assert_eq!(&out, b"hello");
    }

    #[test]
    fn test_consume_and_fill() {
        let mut pool = create_test_pool(1024, 2);
        let mut buf = checkout_with_data(&mut pool, b"abcdefghij");

        let consumed = buf.consume(3);
        assert_eq!(consumed, 3);
        assert_eq!(buf.data(), b"defghij");
        assert_eq!(buf.available_data(), 7);

        let filled = buf.fill(0);
        assert_eq!(filled, 0);
    }

    #[test]
    fn test_empty() {
        let mut pool = create_test_pool(64, 2);
        let buf = pool.checkout().unwrap();
        assert!(buf.empty(), "freshly checked-out buffer should be empty");
    }

    #[test]
    fn test_reset() {
        let mut pool = create_test_pool(1024, 2);
        let mut buf = checkout_with_data(&mut pool, b"data");
        assert!(!buf.empty());

        buf.reset();
        assert!(buf.empty());
        assert_eq!(buf.available_data(), 0);
    }

    #[test]
    fn test_sync() {
        let mut pool = create_test_pool(1024, 2);
        let mut buf = checkout_with_data(&mut pool, b"hello world");
        buf.sync(5, 2);
        assert_eq!(buf.available_data(), 3);
    }

    // -----------------------------------------------------------------------
    // shift() -- unsafe ptr::copy
    // -----------------------------------------------------------------------

    #[test]
    fn test_shift_moves_data_to_start() {
        let mut pool = create_test_pool(1024, 2);
        let mut buf = checkout_with_data(&mut pool, b"hello world");

        buf.inner.position = 5;
        assert_eq!(buf.data(), b" world");

        buf.shift();
        assert_eq!(buf.inner.position, 0);
        assert_eq!(buf.inner.end, 6);
        assert_eq!(buf.data(), b" world");
    }

    #[test]
    fn test_shift_noop_when_position_zero() {
        let mut pool = create_test_pool(1024, 2);
        let mut buf = checkout_with_data(&mut pool, b"hello");

        assert_eq!(buf.inner.position, 0);
        buf.shift();
        assert_eq!(buf.data(), b"hello");
        assert_eq!(buf.inner.position, 0);
        assert_eq!(buf.inner.end, 5);
    }

    #[test]
    fn test_consume_triggers_auto_shift() {
        let mut pool = create_test_pool(256, 2);
        let mut buf = pool.checkout().unwrap();
        let capacity = buf.capacity();

        let fill_count = capacity / 2 + 2;
        let data: Vec<u8> = (0..fill_count as u8).collect();
        let written = buf.write(&data).unwrap();
        assert_eq!(written, fill_count);

        let consume_count = capacity / 2 + 1;
        buf.consume(consume_count);

        assert_eq!(buf.inner.position, 0);
        let remaining = fill_count - consume_count;
        assert_eq!(buf.available_data(), remaining);
    }

    // -----------------------------------------------------------------------
    // delete_slice() -- unsafe ptr::copy
    // -----------------------------------------------------------------------

    #[test]
    fn test_delete_slice_middle() {
        let mut pool = create_test_pool(1024, 2);
        let mut buf = checkout_with_data(&mut pool, b"hello world!");

        let result = buf.delete_slice(3, 5);
        assert!(result.is_some());
        assert_eq!(buf.data(), b"helrld!");
    }

    #[test]
    fn test_delete_slice_from_start() {
        let mut pool = create_test_pool(1024, 2);
        let mut buf = checkout_with_data(&mut pool, b"hello world!");

        let result = buf.delete_slice(0, 3);
        assert!(result.is_some());
        assert_eq!(buf.data(), b"lo world!");
    }

    #[test]
    fn test_delete_slice_near_end() {
        let mut pool = create_test_pool(1024, 2);
        let mut buf = checkout_with_data(&mut pool, b"hello world!");

        let result = buf.delete_slice(7, 4);
        assert!(result.is_some());
        assert_eq!(buf.data(), b"hello w!");
    }

    #[test]
    fn test_delete_slice_out_of_bounds_returns_none() {
        let mut pool = create_test_pool(1024, 2);
        let mut buf = checkout_with_data(&mut pool, b"hello");

        let result = buf.delete_slice(0, 5);
        assert!(result.is_none());

        let result = buf.delete_slice(3, 5);
        assert!(result.is_none());
    }

    #[test]
    fn test_delete_slice_single_byte() {
        let mut pool = create_test_pool(1024, 2);
        let mut buf = checkout_with_data(&mut pool, b"abcd");

        let result = buf.delete_slice(1, 1);
        assert!(result.is_some());
        assert_eq!(buf.data(), b"acd");
    }

    // -----------------------------------------------------------------------
    // replace_slice() -- unsafe ptr::copy
    // -----------------------------------------------------------------------

    #[test]
    fn test_replace_slice_same_size() {
        let mut pool = create_test_pool(1024, 2);
        let mut buf = checkout_with_data(&mut pool, b"hello world");

        let result = buf.replace_slice(b"earth", 6, 5);
        assert!(result.is_some());
        assert_eq!(buf.data(), b"hello earth");
    }

    #[test]
    fn test_replace_slice_shrink() {
        let mut pool = create_test_pool(1024, 2);
        let mut buf = checkout_with_data(&mut pool, b"hello world");

        let result = buf.replace_slice(b"hi", 6, 5);
        assert!(result.is_some());
        assert_eq!(buf.data(), b"hello hi");
    }

    #[test]
    fn test_replace_slice_grow() {
        let mut pool = create_test_pool(1024, 2);
        let mut buf = checkout_with_data(&mut pool, b"hello world");

        let result = buf.replace_slice(b"universe", 6, 5);
        assert!(result.is_some());
        assert_eq!(buf.data(), b"hello universe");
    }

    #[test]
    fn test_replace_slice_at_start() {
        let mut pool = create_test_pool(1024, 2);
        let mut buf = checkout_with_data(&mut pool, b"hello world");

        let result = buf.replace_slice(b"hey", 0, 5);
        assert!(result.is_some());
        assert_eq!(buf.data(), b"hey world");
    }

    #[test]
    fn test_replace_slice_out_of_bounds_returns_none() {
        let mut pool = create_test_pool(1024, 2);
        let mut buf = checkout_with_data(&mut pool, b"hello");

        let result = buf.replace_slice(b"x", 4, 5);
        assert!(result.is_none());
    }

    #[test]
    fn test_replace_slice_exceeds_data_bounds_returns_none() {
        let mut pool = create_test_pool(256, 2);
        let mut buf = checkout_with_data(&mut pool, b"hello");

        let result = buf.replace_slice(b"xyz", 3, 5);
        assert!(result.is_none());
    }

    #[test]
    fn test_replace_slice_replacement_exceeds_capacity_returns_none() {
        let mut pool = create_test_pool(256, 2);
        let mut buf = pool.checkout().unwrap();
        let capacity = buf.capacity();

        let data = vec![b'x'; capacity];
        let written = buf.write(&data).unwrap();
        assert_eq!(written, capacity);

        buf.inner.position = capacity - 2;
        assert_eq!(buf.available_data(), 2);

        // position + start + data_len = (capacity-2) + 0 + 5 = capacity+3 > capacity
        let result = buf.replace_slice(b"abcde", 0, 1);
        assert!(result.is_none());
    }

    // -----------------------------------------------------------------------
    // insert_slice() -- unsafe ptr::copy
    // -----------------------------------------------------------------------

    #[test]
    fn test_insert_slice_at_start() {
        let mut pool = create_test_pool(1024, 2);
        let mut buf = checkout_with_data(&mut pool, b"world");

        let result = buf.insert_slice(b"hello ", 0);
        assert!(result.is_some());
        assert_eq!(buf.data(), b"hello world");
    }

    #[test]
    fn test_insert_slice_in_middle() {
        let mut pool = create_test_pool(1024, 2);
        let mut buf = checkout_with_data(&mut pool, b"helo");

        let result = buf.insert_slice(b"l", 2);
        assert!(result.is_some());
        assert_eq!(buf.data(), b"hello");
    }

    #[test]
    fn test_insert_slice_at_end() {
        let mut pool = create_test_pool(1024, 2);
        let mut buf = checkout_with_data(&mut pool, b"hello");

        let result = buf.insert_slice(b" world", 5);
        assert!(result.is_some());
        assert_eq!(buf.data(), b"hello world");
    }

    #[test]
    fn test_insert_slice_exceeds_capacity_returns_none() {
        let mut pool = create_test_pool(256, 2);
        let mut buf = pool.checkout().unwrap();
        let capacity = buf.capacity();

        let data = vec![b'x'; capacity];
        let written = buf.write(&data).unwrap();
        assert_eq!(written, capacity);
        assert_eq!(buf.available_space(), 0);

        let result = buf.insert_slice(b"y", 0);
        assert!(result.is_none());
    }

    #[test]
    fn test_insert_slice_beyond_data_returns_none() {
        let mut pool = create_test_pool(1024, 2);
        let mut buf = checkout_with_data(&mut pool, b"hello");

        let result = buf.insert_slice(b"x", 6);
        assert!(result.is_none());
    }

    // -----------------------------------------------------------------------
    // Combined operations -- exercise multiple unsafe paths together
    // -----------------------------------------------------------------------

    #[test]
    fn test_write_consume_shift_write_again() {
        let mut pool = create_test_pool(32, 2);
        let mut buf = checkout_with_data(&mut pool, b"first");

        buf.consume(5);
        assert_eq!(buf.available_data(), 0);

        let n = buf.write(b"second").unwrap();
        assert_eq!(n, 6);
        assert_eq!(buf.data(), b"second");
    }

    #[test]
    fn test_delete_then_insert() {
        let mut pool = create_test_pool(1024, 2);
        let mut buf = checkout_with_data(&mut pool, b"hello cruel world");

        buf.delete_slice(6, 6);
        assert_eq!(buf.data(), b"hello world");

        buf.insert_slice(b"beautiful ", 6);
        assert_eq!(buf.data(), b"hello beautiful world");
    }

    #[test]
    fn test_multiple_replace_operations() {
        let mut pool = create_test_pool(1024, 2);
        let mut buf = checkout_with_data(&mut pool, b"aXbXc");

        buf.replace_slice(b"12", 1, 1);
        assert_eq!(buf.data(), b"a12bXc");

        buf.replace_slice(b"34", 4, 1);
        assert_eq!(buf.data(), b"a12b34c");
    }
}
