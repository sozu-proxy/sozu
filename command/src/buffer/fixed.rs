use std::{cmp, ptr};
use std::io::{self,Write,Read};
use poule::Reset;

#[derive(Debug,PartialEq,Clone,Default)]
pub struct BufferMetadata {
  position: usize,
  end:      usize
}

#[derive(Debug,PartialEq,Clone)]
pub struct Buffer {
  inner: trailer::Trailer<BufferMetadata>,
}

impl Buffer {
  pub fn with_capacity(capacity: usize) -> Buffer {
    let mut inner: trailer::Trailer<BufferMetadata> = trailer::Trailer::new(capacity);
    inner.position = 0;
    inner.end = 0;

    Buffer { inner }
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
    if self.inner.capacity() >= new_size {
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
    self.inner.end - self.inner.position
  }

  pub fn available_space(&self) -> usize {
    self.inner.capacity() - self.inner.end
  }

  pub fn capacity(&self) -> usize {
    self.inner.capacity()
  }

  pub fn empty(&self) -> bool {
    self.inner.position == self.inner.end
  }

  pub fn consume(&mut self, count: usize) -> usize {
    let cnt = cmp::min(count, self.available_data());
    self.inner.position += cnt;
    if self.inner.position > self.inner.capacity() / 2 {
      //trace!("consume shift: pos {}, end {}", self.position, self.end);
      self.shift();
    }
    cnt
  }

  pub fn fill(&mut self, count: usize) -> usize {
    let cnt = cmp::min(count, self.available_space());
    self.inner.end += cnt;
    if self.available_space() < self.available_data() + cnt {
      //trace!("fill shift: pos {}, end {}", self.position, self.end);
      self.shift();
    }

    cnt
  }

  pub fn reset(&mut self) {
    self.inner.position = 0;
    self.inner.end      = 0;
  }

  pub fn data(&self) -> &[u8] {
    &self.inner.bytes()[self.inner.position..self.inner.end]
  }

  pub fn space(&mut self) -> &mut[u8] {
    let range = self.inner.end..self.inner.capacity();
    &mut self.inner.bytes_mut()[range]
  }

  pub fn shift(&mut self) {
    if self.inner.position > 0 {
      unsafe {
        let length = self.inner.end - self.inner.position;
        ptr::copy( (&self.inner.bytes()[self.inner.position..self.inner.end]).as_ptr(), (&mut self.inner.bytes_mut()[..length]).as_mut_ptr(), length);
        self.inner.position = 0;
        self.inner.end      = length;
      }
    }
  }

  pub fn delete_slice(&mut self, start: usize, length: usize) -> Option<usize> {
    if start + length >= self.available_data() {
      return None
    }

    unsafe {
      let begin = self.inner.position + start;
      let next_end = self.inner.end - length;
      ptr::copy(
        (&self.inner.bytes()[begin+length..self.inner.end]).as_ptr(),
        (&mut self.inner.bytes_mut()[begin..next_end]).as_mut_ptr(),
        self.inner.end - (begin+length)
      );
      self.inner.end = next_end;
    }
    Some(self.available_data())
  }

  pub fn replace_slice(&mut self, data: &[u8], start: usize, length: usize) -> Option<usize> {
    let data_len = data.len();
    if start + length > self.available_data() ||
      self.inner.position + start + data_len > self.capacity() {
      return None
    }

    unsafe {
      let begin = self.inner.position + start;
      let slice_end = begin + data_len;
      // we reduced the data size
      if data_len < length {
        ptr::copy(data.as_ptr(), (&mut self.inner.bytes_mut()[begin..slice_end]).as_mut_ptr(), data_len);

        ptr::copy((&self.inner.bytes()[start+length..self.inner.end]).as_ptr(), (&mut self.inner.bytes_mut()[slice_end..]).as_mut_ptr(), self.inner.end - (start + length));
        self.inner.end -= length - data_len;

      // we put more data in the buffer
      } else {
        ptr::copy((&self.inner.bytes()[start+length..self.inner.end]).as_ptr(), (&mut self.inner.bytes_mut()[start+data_len..]).as_mut_ptr(), self.inner.end - (start + length));
        ptr::copy(data.as_ptr(), (&mut self.inner.bytes_mut()[begin..slice_end]).as_mut_ptr(), data_len);
        self.inner.end += data_len - length;
      }
    }
    Some(self.available_data())
  }

  pub fn insert_slice(&mut self, data: &[u8], start: usize) -> Option<usize> {
    let data_len = data.len();
    if start > self.available_data() ||
      self.inner.position + self.inner.end + data_len > self.capacity() {
      return None
    }

    unsafe {
      let begin = self.inner.position + start;
      let slice_end = begin + data_len;
      ptr::copy((&self.inner.bytes()[start..self.inner.end]).as_ptr(), (&mut self.inner.bytes_mut()[start+data_len..]).as_mut_ptr(), self.inner.end - start);
      ptr::copy(data.as_ptr(), (&mut self.inner.bytes_mut()[begin..slice_end]).as_mut_ptr(), data_len);
      self.inner.end += data_len;
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
      ptr::copy((&self.inner.bytes()[self.inner.position..self.inner.position+len]).as_ptr(), buf.as_mut_ptr(), len);
      self.inner.position += len;
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
