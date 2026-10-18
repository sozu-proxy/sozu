//! Growable ring buffer (`Buffer`).
//!
//! Same `position` / `end` / `capacity` discipline as `fixed.rs` but
//! grows the backing `Vec<u8>` on demand up to a configured cap. Shift /
//! insert / replace operations use overlapping-safe `std::ptr::copy`
//! (NOT `copy_nonoverlapping`) with bounds checked against
//! `self.capacity()`.
//!
//! The buffer carries command-channel frames, some of which hold protected
//! values, so it keeps one invariant: every byte of `memory` outside the
//! pending window `position..end` is zero. Consuming, reading, resetting,
//! shifting, deleting or replacing erases the bytes that stop being pending,
//! and growing or shrinking copies the pending window into a fresh zeroed
//! allocation and erases the old one before releasing it, instead of letting
//! `Vec::resize` / `Vec::shrink_to_fit` free it unwiped. `Drop` erases what is
//! still pending, and `Debug` prints offsets only.

use std::{
    cmp, fmt,
    io::{self, Read, Write},
    mem, ptr,
};

use pool::Reset;
use zeroize::Zeroize;

#[derive(PartialEq, Eq, Clone)]
pub struct Buffer {
    memory: Vec<u8>,
    capacity: usize,
    position: usize,
    end: usize,
}

impl fmt::Debug for Buffer {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Buffer")
            .field("capacity", &self.capacity)
            .field("position", &self.position)
            .field("end", &self.end)
            .finish_non_exhaustive()
    }
}

impl Drop for Buffer {
    fn drop(&mut self) {
        self.memory.zeroize();
    }
}

impl Buffer {
    pub fn with_capacity(capacity: usize) -> Buffer {
        Buffer {
            memory: vec![0; capacity],
            capacity,
            position: 0,
            end: 0,
        }
    }

    pub fn from_slice(sl: &[u8]) -> Buffer {
        Buffer {
            memory: Vec::from(sl),
            capacity: sl.len(),
            position: 0,
            end: sl.len(),
        }
    }

    pub fn grow(&mut self, new_size: usize) -> bool {
        if self.capacity >= new_size {
            return false;
        }

        self.reallocate(new_size);
        self.capacity = new_size;
        true
    }

    /// Shrink the buffer to target_size, preserving any pending data.
    /// If pending data exceeds target_size, the buffer is not shrunk.
    pub fn shrink(&mut self, target_size: usize) -> bool {
        if target_size >= self.capacity {
            return false;
        }
        self.shift();
        if self.end > target_size {
            return false;
        }
        self.reallocate(target_size);
        self.capacity = target_size;
        true
    }

    /// Move the pending window `position..end` to a fresh zeroed allocation of
    /// `size` bytes, at the same offsets, and erase the old allocation before
    /// it is released.
    ///
    /// `Vec::resize` and `Vec::shrink_to_fit` may free the old block with the
    /// pending bytes still in it, and nothing can erase a block that is gone.
    fn reallocate(&mut self, size: usize) {
        debug_assert!(
            self.end <= size,
            "the pending window must fit the new allocation"
        );
        debug_assert!(
            self.position <= self.end,
            "the pending window must not be inverted"
        );
        let mut destination = vec![0; size];
        destination[self.position..self.end].copy_from_slice(&self.memory[self.position..self.end]);
        let mut previous = mem::replace(&mut self.memory, destination);
        previous.zeroize();
    }

    /// Erase `memory[from..to]`, which has just stopped being pending.
    fn erase(&mut self, from: usize, to: usize) {
        debug_assert!(from <= to, "the erased range must not be inverted");
        debug_assert!(
            to <= self.memory.len(),
            "the erased range must be in bounds"
        );
        self.memory[from..to].zeroize();
    }

    pub fn available_data(&self) -> usize {
        self.end - self.position
    }

    pub fn available_space(&self) -> usize {
        self.capacity - self.end
    }

    pub fn capacity(&self) -> usize {
        self.capacity
    }

    pub fn empty(&self) -> bool {
        self.position == self.end
    }

    pub fn consume(&mut self, count: usize) -> usize {
        let cnt = cmp::min(count, self.available_data());
        self.erase(self.position, self.position + cnt);
        self.position += cnt;
        if self.position > self.capacity / 2 {
            //trace!("consume shift: pos {}, end {}", self.position, self.end);
            self.shift();
        }
        cnt
    }

    /// displaces the end pointer to the right
    pub fn fill(&mut self, count: usize) -> usize {
        let cnt = cmp::min(count, self.available_space());
        self.end += cnt;
        if self.available_space() < self.available_data() + cnt {
            //trace!("fill shift: pos {}, end {}", self.position, self.end);
            self.shift();
        }

        cnt
    }

    pub fn reset(&mut self) {
        self.erase(self.position, self.end);
        self.position = 0;
        self.end = 0;
    }

    pub fn data(&self) -> &[u8] {
        &self.memory[self.position..self.end]
    }

    pub fn space(&mut self) -> &mut [u8] {
        &mut self.memory[self.end..self.capacity]
    }

    pub fn shift(&mut self) {
        if self.position > 0 {
            let previous_end = self.end;
            // SAFETY: src and dst point into `self.memory` (same
            // allocation); the slice indexing above bounds-checks both
            // ranges (`position..end` and `..length`) against the live
            // buffer length. `ptr::copy` is overlap-safe.
            unsafe {
                let length = self.end - self.position;
                ptr::copy(
                    self.memory[self.position..self.end].as_ptr(),
                    self.memory[..length].as_mut_ptr(),
                    length,
                );
                self.position = 0;
                self.end = length;
            }
            // The window moved down; what it used to cover above its new end
            // is a stale copy.
            self.erase(self.end, previous_end);
        }
    }

    pub fn delete_slice(&mut self, start: usize, length: usize) -> Option<usize> {
        if start + length >= self.available_data() {
            return None;
        }

        let previous_end = self.end;
        // SAFETY: src and dst point into `self.memory` (same allocation).
        // The early-return above guarantees `start + length < available_data`,
        // and slice indexing bounds-checks both `begin+length..end` and
        // `begin..next_end` against the live buffer length. `ptr::copy` is
        // overlap-safe.
        unsafe {
            let begin = self.position + start;
            let next_end = self.end - length;
            ptr::copy(
                self.memory[begin + length..self.end].as_ptr(),
                self.memory[begin..next_end].as_mut_ptr(),
                self.end - (begin + length),
            );
            self.end = next_end;
        }
        // The tail moved down over the deleted bytes; its old position above
        // the new end is a stale copy.
        self.erase(self.end, previous_end);
        Some(self.available_data())
    }

    pub fn replace_slice(&mut self, data: &[u8], start: usize, length: usize) -> Option<usize> {
        let data_len = data.len();
        if start + length > self.available_data()
            || self.position + start + data_len > self.capacity
        {
            return None;
        }

        let previous_end = self.end;
        // SAFETY: every `ptr::copy` below moves bytes inside `self.memory`
        // (same allocation) or copies from the caller's `data` slice into
        // it. The two early-return checks above bound the affected ranges
        // against `available_data()` and `self.capacity`, and each slice
        // indexing site is bounds-checked. `ptr::copy` is overlap-safe.
        unsafe {
            let begin = self.position + start;
            let slice_end = begin + data_len;
            // we reduced the data size
            if data_len < length {
                ptr::copy(
                    data.as_ptr(),
                    self.memory[begin..slice_end].as_mut_ptr(),
                    data_len,
                );

                ptr::copy(
                    self.memory[start + length..self.end].as_ptr(),
                    self.memory[slice_end..].as_mut_ptr(),
                    self.end - (start + length),
                );
                self.end -= length - data_len;

            // we put more data in the buffer
            } else {
                ptr::copy(
                    self.memory[start + length..self.end].as_ptr(),
                    self.memory[start + data_len..].as_mut_ptr(),
                    self.end - (start + length),
                );
                ptr::copy(
                    data.as_ptr(),
                    self.memory[begin..slice_end].as_mut_ptr(),
                    data_len,
                );
                self.end += data_len - length;
            }
        }
        // A reduced window leaves its old tail above the new end; a grown one
        // has nothing to erase (`previous_end <= end`).
        if self.end < previous_end {
            self.erase(self.end, previous_end);
        }
        Some(self.available_data())
    }

    pub fn insert_slice(&mut self, data: &[u8], start: usize) -> Option<usize> {
        let data_len = data.len();
        if start > self.available_data() || self.position + self.end + data_len > self.capacity {
            return None;
        }

        // SAFETY: both `ptr::copy` calls touch `self.memory` (same
        // allocation) or copy from `data` into it. The early-return checks
        // bound `start <= available_data` and the resulting tail
        // `position + end + data_len <= capacity`, and each slice indexing
        // site is bounds-checked. `ptr::copy` is overlap-safe.
        unsafe {
            let begin = self.position + start;
            let slice_end = begin + data_len;
            ptr::copy(
                self.memory[start..self.end].as_ptr(),
                self.memory[start + data_len..].as_mut_ptr(),
                self.end - start,
            );
            ptr::copy(
                data.as_ptr(),
                self.memory[begin..slice_end].as_mut_ptr(),
                data_len,
            );
            self.end += data_len;
        }
        Some(self.available_data())
    }
}

impl Write for Buffer {
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

impl Read for Buffer {
    fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
        let len = cmp::min(self.available_data(), buf.len());
        // SAFETY: `len = min(available_data, buf.len())`, so the source
        // range `position..position+len` lies inside `self.memory` and the
        // destination `buf[..len]` fits the caller's `&mut [u8]`. The two
        // slices are in different allocations; `ptr::copy` is overlap-safe
        // regardless.
        unsafe {
            ptr::copy(
                self.memory[self.position..self.position + len].as_ptr(),
                buf.as_mut_ptr(),
                len,
            );
        }
        // The caller owns the copy now; the buffer keeps none of it.
        self.erase(self.position, self.position + len);
        self.position += len;
        Ok(len)
    }
}

impl Reset for Buffer {
    fn reset(&mut self) {
        self.reset();
    }
}

#[cfg(test)]
mod tests {
    use std::io::Write;

    use super::*;

    #[test]
    fn fill_and_consume() {
        let mut b = Buffer::with_capacity(10);
        assert_eq!(b.available_data(), 0);
        assert_eq!(b.available_space(), 10);
        let res = b.write(&b"abcd"[..]);
        assert_eq!(res.ok(), Some(4));
        assert_eq!(b.available_data(), 4);
        assert_eq!(b.available_space(), 6);

        assert_eq!(b.data(), &b"abcd"[..]);

        b.consume(2);
        assert_eq!(b.available_data(), 2);
        assert_eq!(b.available_space(), 6);
        assert_eq!(b.data(), &b"cd"[..]);

        b.shift();
        assert_eq!(b.available_data(), 2);
        assert_eq!(b.available_space(), 8);
        assert_eq!(b.data(), &b"cd"[..]);

        assert_eq!(b.write(&b"efghijklmnop"[..]).ok(), Some(8));
        assert_eq!(b.available_data(), 10);
        assert_eq!(b.available_space(), 0);
        assert_eq!(b.data(), &b"cdefghijkl"[..]);
        b.shift();
        assert_eq!(b.available_data(), 10);
        assert_eq!(b.available_space(), 0);
        assert_eq!(b.data(), &b"cdefghijkl"[..]);
    }

    #[test]
    fn delete() {
        let mut b = Buffer::with_capacity(10);
        let _ = b.write(&b"abcdefgh"[..]).expect("should write");
        assert_eq!(b.available_data(), 8);
        assert_eq!(b.available_space(), 2);

        assert_eq!(b.delete_slice(2, 3), Some(5));
        assert_eq!(b.available_data(), 5);
        assert_eq!(b.available_space(), 5);
        assert_eq!(b.data(), &b"abfgh"[..]);

        assert_eq!(b.delete_slice(5, 2), None);
        assert_eq!(b.delete_slice(4, 2), None);
    }

    #[test]
    fn replace() {
        let mut b = Buffer::with_capacity(10);
        let _ = b.write(&b"abcdefgh"[..]).expect("should write");
        assert_eq!(b.available_data(), 8);
        assert_eq!(b.available_space(), 2);

        assert_eq!(b.replace_slice(&b"ABC"[..], 2, 3), Some(8));
        assert_eq!(b.available_data(), 8);
        assert_eq!(b.available_space(), 2);
        assert_eq!(b.data(), &b"abABCfgh"[..]);

        assert_eq!(b.replace_slice(&b"XYZ"[..], 8, 3), None);
        assert_eq!(b.replace_slice(&b"XYZ"[..], 6, 3), None);

        assert_eq!(b.replace_slice(&b"XYZ"[..], 2, 4), Some(7));
        assert_eq!(b.available_data(), 7);
        assert_eq!(b.available_space(), 3);
        assert_eq!(b.data(), &b"abXYZgh"[..]);

        assert_eq!(b.replace_slice(&b"123"[..], 2, 2), Some(8));
        assert_eq!(b.available_data(), 8);
        assert_eq!(b.available_space(), 2);
        assert_eq!(b.data(), &b"ab123Zgh"[..]);
    }
}
