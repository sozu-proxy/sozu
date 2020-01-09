use std::{cmp, ptr};
use std::io::{self,Write,Read};
use std::iter::repeat;
use pool::Reset;

#[derive(Debug,PartialEq,Clone)]
pub struct Buffer {
  memory:   Vec<u8>,
  capacity: usize,
  position: usize,
  end:      usize
}

impl Buffer {
  pub fn with_capacity(capacity: usize) -> Buffer {
    let mut v = Vec::with_capacity(capacity);
    v.extend(repeat(0).take(capacity));
    Buffer {
      memory:   v,
      capacity,
      position: 0,
      end:      0
    }
  }

  pub fn from_slice(sl: &[u8]) -> Buffer {
    Buffer {
      memory:   Vec::from(sl),
      capacity: sl.len(),
      position: 0,
      end:      sl.len()
    }
  }

  pub fn grow(&mut self, new_size: usize) -> bool {
    if self.capacity >= new_size {
      return false;
    }

    self.memory.resize(new_size, 0);
    self.capacity = new_size;
    true
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
    let cnt        = cmp::min(count, self.available_data());
    self.position += cnt;
    if self.position > self.capacity / 2 {
      //trace!("consume shift: pos {}, end {}", self.position, self.end);
      self.shift();
    }
    cnt
  }

  pub fn fill(&mut self, count: usize) -> usize {
    let cnt   = cmp::min(count, self.available_space());
    self.end += cnt;
    if self.available_space() < self.available_data() + cnt {
      //trace!("fill shift: pos {}, end {}", self.position, self.end);
      self.shift();
    }

    cnt
  }

  pub fn reset(&mut self) {
    self.position = 0;
    self.end      = 0;
  }

  pub fn data(&self) -> &[u8] {
    &self.memory[self.position..self.end]
  }

  pub fn space(&mut self) -> &mut[u8] {
    &mut self.memory[self.end..self.capacity]
  }

  pub fn shift(&mut self) {
    if self.position > 0 {
      unsafe {
        let length = self.end - self.position;
        ptr::copy( (&self.memory[self.position..self.end]).as_ptr(), (&mut self.memory[..length]).as_mut_ptr(), length);
        self.position = 0;
        self.end      = length;
      }
    }
  }

  pub fn delete_slice(&mut self, start: usize, length: usize) -> Option<usize> {
    if start + length >= self.available_data() {
      return None
    }

    unsafe {
      let begin    = self.position + start;
      let next_end = self.end - length;
      ptr::copy(
        (&self.memory[begin+length..self.end]).as_ptr(),
        (&mut self.memory[begin..next_end]).as_mut_ptr(),
        self.end - (begin+length)
      );
      self.end = next_end;
    }
    Some(self.available_data())
  }

  pub fn replace_slice(&mut self, data: &[u8], start: usize, length: usize) -> Option<usize> {
    let data_len = data.len();
    if start + length > self.available_data() ||
      self.position + start + data_len > self.capacity {
      return None
    }

    unsafe {
      let begin     = self.position + start;
      let slice_end = begin + data_len;
      // we reduced the data size
      if data_len < length {
        ptr::copy(data.as_ptr(), (&mut self.memory[begin..slice_end]).as_mut_ptr(), data_len);

        ptr::copy((&self.memory[start+length..self.end]).as_ptr(), (&mut self.memory[slice_end..]).as_mut_ptr(), self.end - (start + length));
        self.end -= length - data_len;

      // we put more data in the buffer
      } else {
        ptr::copy((&self.memory[start+length..self.end]).as_ptr(), (&mut self.memory[start+data_len..]).as_mut_ptr(), self.end - (start + length));
        ptr::copy(data.as_ptr(), (&mut self.memory[begin..slice_end]).as_mut_ptr(), data_len);
        self.end += data_len - length;
      }
    }
    Some(self.available_data())
  }

  pub fn insert_slice(&mut self, data: &[u8], start: usize) -> Option<usize> {
    let data_len = data.len();
    if start > self.available_data() ||
      self.position + self.end + data_len > self.capacity {
      return None
    }

    unsafe {
      let begin     = self.position + start;
      let slice_end = begin + data_len;
      ptr::copy((&self.memory[start..self.end]).as_ptr(), (&mut self.memory[start+data_len..]).as_mut_ptr(), self.end - start);
      ptr::copy(data.as_ptr(), (&mut self.memory[begin..slice_end]).as_mut_ptr(), data_len);
      self.end += data_len;
    }
    Some(self.available_data())
  }
}

impl Write for Buffer {
  fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
    match self.space().write(buf) {
      Ok(size) => { self.fill(size); Ok(size) },
      err      => err
    }
  }

  fn flush(&mut self) -> io::Result<()> {
    Ok(())
  }
}

impl Read for Buffer {
  fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
    let len = cmp::min(self.available_data(), buf.len());
    unsafe {
      ptr::copy((&self.memory[self.position..self.position+len]).as_ptr(), buf.as_mut_ptr(), len);
      self.position += len;
    }
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
  use super::*;
  use std::io::Write;

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
