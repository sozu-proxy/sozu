//! Fixed-capacity ring buffer (`Buffer`).
//!
//! Tracks `position` (read cursor), `end` (write cursor), and `capacity`
//! over a `Vec<u8>` allocated once at construction.

use std::{
    cmp,
    io::{self, Read, Write},
};

use poule::Reset;

#[derive(Debug, PartialEq, Clone)]
pub struct Buffer {
    memory: Vec<u8>,
    position: usize,
    end: usize,
}

impl Buffer {
    pub fn with_capacity(capacity: usize) -> Buffer {
        Buffer {
            memory: vec![0; capacity],
            position: 0,
            end: 0,
        }
    }

    /*pub fn from_slice(sl: &[u8]) -> Buffer {
      Buffer {
        memory:   Vec::from(sl),
        capacity: sl.len(),
        position: 0,
        end:      sl.len()
      }
      unimplemented!()
    }
      */

    pub fn grow(&mut self, new_size: usize) -> bool {
        if self.capacity() >= new_size {
            return false;
        }

        /*
        self.memory.resize(new_size, 0);
        self.capacity = new_size;
        true
          */
        unimplemented!()
    }

    pub fn available_data(&self) -> usize {
        self.end - self.position
    }

    pub fn available_space(&self) -> usize {
        self.capacity() - self.end
    }

    pub fn capacity(&self) -> usize {
        self.memory.len()
    }

    pub fn empty(&self) -> bool {
        self.position == self.end
    }

    pub fn consume(&mut self, count: usize) -> usize {
        let cnt = cmp::min(count, self.available_data());
        self.position += cnt;
        if self.position > self.capacity() / 2 {
            //trace!("consume shift: pos {}, end {}", self.position, self.end);
            self.shift();
        }
        cnt
    }

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
        self.position = 0;
        self.end = 0;
    }

    pub fn data(&self) -> &[u8] {
        &self.memory[self.position..self.end]
    }

    pub fn space(&mut self) -> &mut [u8] {
        &mut self.memory[self.end..]
    }

    pub fn shift(&mut self) {
        if self.position > 0 {
            let length = self.end - self.position;
            self.memory.copy_within(self.position..self.end, 0);
            self.position = 0;
            self.end = length;
        }
    }

    pub fn delete_slice(&mut self, start: usize, length: usize) -> Option<usize> {
        let end = start.checked_add(length)?;
        if end >= self.available_data() {
            return None;
        }

        let begin = self.position + start;
        let tail_start = begin + length;
        self.memory.copy_within(tail_start..self.end, begin);
        self.end -= length;
        Some(self.available_data())
    }

    pub fn replace_slice(&mut self, data: &[u8], start: usize, length: usize) -> Option<usize> {
        let data_len = data.len();
        let replaced_end = start.checked_add(length)?;
        if replaced_end > self.available_data() {
            return None;
        }

        let begin = self.position + start;
        let tail_start = begin + length;
        let slice_end = begin + data_len;

        match data_len.cmp(&length) {
            cmp::Ordering::Less => {
                self.memory[begin..slice_end].copy_from_slice(data);
                self.memory.copy_within(tail_start..self.end, slice_end);
                self.end -= length - data_len;
            }
            cmp::Ordering::Equal => {
                self.memory[begin..slice_end].copy_from_slice(data);
            }
            cmp::Ordering::Greater => {
                let new_end = self.end.checked_add(data_len - length)?;
                if new_end > self.capacity() {
                    return None;
                }
                self.memory.copy_within(tail_start..self.end, slice_end);
                self.memory[begin..slice_end].copy_from_slice(data);
                self.end = new_end;
            }
        }
        Some(self.available_data())
    }

    pub fn insert_slice(&mut self, data: &[u8], start: usize) -> Option<usize> {
        self.replace_slice(data, start, 0)
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
        buf[..len].copy_from_slice(&self.memory[self.position..self.position + len]);
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
