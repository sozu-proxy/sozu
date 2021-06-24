/// experimental module to measure buffer pool usage
///
/// this allows us to track how many buffers are used through the
/// buffers.count metric.
///
/// Right now, we wrap the `pool` crate, but we might write a different
/// buffer pool in the future, so this module will still be useful to
/// test the differences

use poule;
use std::{cmp, ops, ptr};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::io::{self, Write, Read};

static BUFFER_COUNT: AtomicUsize = AtomicUsize::new(0);

pub struct Pool {
  pub inner: poule::Pool<BufferMetadata>,
  pub buffer_size: usize,
}

impl Pool {
  pub fn with_capacity(minimum: usize, maximum: usize, buffer_size: usize) -> Pool {
    let mut inner = poule::Pool::with_extra(maximum, buffer_size);
    inner.grow_to(minimum);
    Pool {
      inner,
      buffer_size,
    }
  }

  pub fn checkout(&mut self) -> Option<Checkout> {
    if self.inner.used() == self.inner.capacity() &&
        self.inner.capacity() < self.inner.maximum_capacity() {
        self.inner.grow_to(std::cmp::min(self.inner.capacity()*2, self.inner.maximum_capacity()));
        debug!("growing pool capacity from {} to {}", self.inner.capacity(),
          std::cmp::min(self.inner.capacity()*2, self.inner.maximum_capacity()));
    }
    let capacity = self.buffer_size;
    self.inner.checkout(|| {
        trace!("initializing a buffer with capacity {}", capacity);
        BufferMetadata::new()
    }).map(|c| {
      let old_buffer_count = BUFFER_COUNT.fetch_add(1, Ordering::SeqCst);
      gauge!("buffer.count", old_buffer_count + 1);
      Checkout {
        inner: c
      }
    })
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

#[derive(Debug,PartialEq,Clone)]
pub struct BufferMetadata {
  position: usize,
  end:      usize
}

impl BufferMetadata {
  pub fn new() -> BufferMetadata {
      BufferMetadata {
          position: 0,
          end: 0,
      }
  }
}

impl poule::Reset for BufferMetadata {
  fn reset(&mut self) {
    self.position = 0;
    self.end = 0;
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
      gauge!("buffer.count", old_buffer_count - 1);
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

  pub fn consume(&mut self, count: usize) -> usize {
    let cnt = cmp::min(count, self.available_data());
    self.inner.position += cnt;
    if self.inner.position > self.capacity() / 2 {
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
    &self.inner.extra()[self.inner.position..self.inner.end]
  }

  pub fn space(&mut self) -> &mut[u8] {
    let range = self.inner.end..self.capacity();
    &mut self.inner.extra_mut()[range]
  }

  pub fn shift(&mut self) {
    let pos = self.inner.position;
    let end = self.inner.end;
    if pos > 0 {
      unsafe {
        let length = end - pos;
        ptr::copy( (&self.inner.extra()[pos..end]).as_ptr(),
          (&mut self.inner.extra_mut()[..length]).as_mut_ptr(), length);
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
        (&self.inner.extra()[begin+length..self.inner.end]).as_ptr(),
        (&mut self.inner.extra_mut()[begin..next_end]).as_mut_ptr(),
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
        ptr::copy(data.as_ptr(), (&mut self.inner.extra_mut()[begin..slice_end]).as_mut_ptr(), data_len);

        ptr::copy((&self.inner.extra()[start+length..self.inner.end]).as_ptr(),
          (&mut self.inner.extra_mut()[slice_end..]).as_mut_ptr(), self.inner.end - (start + length));
        self.inner.end -= length - data_len;

      // we put more data in the buffer
      } else {
        ptr::copy((&self.inner.extra()[start+length..self.inner.end]).as_ptr(),
          (&mut self.inner.extra_mut()[start+data_len..]).as_mut_ptr(), self.inner.end - (start + length));
        ptr::copy(data.as_ptr(), (&mut self.inner.extra_mut()[begin..slice_end]).as_mut_ptr(), data_len);
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
      ptr::copy((&self.inner.extra()[start..self.inner.end]).as_ptr(), (&mut self.inner.extra_mut()[start+data_len..]).as_mut_ptr(), self.inner.end - start);
      ptr::copy(data.as_ptr(), (&mut self.inner.extra_mut()[begin..slice_end]).as_mut_ptr(), data_len);
      self.inner.end += data_len;
    }
    Some(self.available_data())
  }
}

impl Write for Checkout {
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

impl Read for Checkout {
  fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
    let len = cmp::min(self.available_data(), buf.len());
    unsafe {
      ptr::copy((&self.inner.extra()[self.inner.position..self.inner.position+len]).as_ptr(),
        buf.as_mut_ptr(), len);
      self.inner.position += len;
    }
    Ok(len)
  }
}
