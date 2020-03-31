/// experimental module to measure buffer pool usage
///
/// this allows us to track how many buffers are used through the
/// buffers.count metric.
///
/// Right now, we wrap the `pool` crate, but we might write a different
/// buffer pool in the future, so this module will still be useful to
/// test the differences

use poule;
use std::ops;
use std::sync::atomic::{AtomicUsize, Ordering};
use sozu_command::buffer::fixed::Buffer;

static BUFFER_COUNT: AtomicUsize = AtomicUsize::new(0);

pub type Reset = dyn poule::Reset;

pub struct Pool {
  pub inner: poule::Pool<Buffer>,
  pub buffer_size: usize,
}

impl Pool {
  pub fn with_capacity(count: usize, buffer_size: usize) -> Pool {
    let mut inner = poule::Pool::with_capacity(count);
    inner.grow_to(1);
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
        Buffer::with_capacity(capacity)
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
  type Target = poule::Pool<Buffer>;

  fn deref(&self) -> &Self::Target {
    &self.inner
  }
}

impl ops::DerefMut for Pool {
  fn deref_mut(&mut self) -> &mut poule::Pool<Buffer> {
    &mut self.inner
  }
}

pub struct Checkout {
  pub inner: poule::Checkout<Buffer>,
}

impl ops::Deref for Checkout {
    type Target = poule::Checkout<Buffer>;

    fn deref(&self) -> &Self::Target {
        &self.inner
    }
}

impl ops::DerefMut for Checkout {
    fn deref_mut(&mut self) -> &mut poule::Checkout<Buffer> {
        &mut self.inner
    }
}

impl Drop for Checkout {
    fn drop(&mut self) {
      let old_buffer_count = BUFFER_COUNT.fetch_sub(1, Ordering::SeqCst);
      gauge!("buffer.count", old_buffer_count - 1);
    }
}

/*
unsafe impl<T: Send> Send for Checkout<T> { }
unsafe impl<T: Sync> Sync for Checkout<T> { }
*/
