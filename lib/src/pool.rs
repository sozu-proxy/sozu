/// experimental module to measure buffer pool usage
///
/// this allows us to track how many buffers are used through the
/// buffers.count metric.
///
/// Right now, we wrap the `pool` crate, but we might write a different
/// buffer pool in the future, so this module will still be useful to
/// test the differences


use pool_crate;
use std::ops;
use std::sync::atomic::{AtomicUsize, Ordering};

static BUFFER_COUNT: AtomicUsize = AtomicUsize::new(0);

pub type Reset = dyn pool_crate::Reset;

pub struct Pool<T:pool_crate::Reset> {
  pub inner: pool_crate::Pool<T>,
}

impl<T: pool_crate::Reset> Pool<T> {
  pub fn with_capacity<F>(count: usize, extra: usize, init: F) -> Pool<T>
    where F: Fn() -> T {
    Pool {
      inner: pool_crate::Pool::with_capacity(count, extra, init),
    }
  }

  pub fn checkout(&mut self) -> Option<Checkout<T>> {
    self.inner.checkout().map(|c| {
      let old_buffer_count = BUFFER_COUNT.fetch_add(1, Ordering::SeqCst);
      gauge!("buffer.count", old_buffer_count + 1);
      Checkout {
        inner: c
      }
    })
  }
}

impl<T: pool_crate::Reset> ops::Deref for Pool<T> {
  type Target = pool_crate::Pool<T>;

  fn deref(&self) -> &Self::Target {
    &self.inner
  }
}

impl<T: pool_crate::Reset> ops::DerefMut for Pool<T> {
  fn deref_mut(&mut self) -> &mut pool_crate::Pool<T> {
    &mut self.inner
  }
}

pub struct Checkout<T> {
  pub inner: pool_crate::Checkout<T>,
}

impl<T> ops::Deref for Checkout<T> {
    type Target = pool_crate::Checkout<T>;

    fn deref(&self) -> &Self::Target {
        &self.inner
    }
}

impl<T> ops::DerefMut for Checkout<T> {
    fn deref_mut(&mut self) -> &mut pool_crate::Checkout<T> {
        &mut self.inner
    }
}

impl<T> Drop for Checkout<T> {
    fn drop(&mut self) {
      let old_buffer_count = BUFFER_COUNT.fetch_sub(1, Ordering::SeqCst);
      gauge!("buffer.count", old_buffer_count - 1);
    }
}

unsafe impl<T: Send> Send for Checkout<T> { }
unsafe impl<T: Sync> Sync for Checkout<T> { }
