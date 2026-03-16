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

static BUFFER_COUNT: AtomicUsize = AtomicUsize::new(0);

pub struct Pool {
    pub inner: poule::Pool<BufferMetadata>,
    pub buffer_size: usize,
}

impl Pool {
    pub fn with_capacity(minimum: usize, maximum: usize, buffer_size: usize) -> Pool {
        let mut inner = poule::Pool::with_extra(maximum, buffer_size);
        inner.grow_to(minimum);
        Pool { inner, buffer_size }
    }

    pub fn checkout(&mut self) -> Option<Checkout> {
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
        self.inner
            .checkout(|| {
                trace!("initializing a buffer with capacity {}", capacity);
                BufferMetadata::new()
            })
            .map(|c| {
                let old_buffer_count = BUFFER_COUNT.fetch_add(1, Ordering::SeqCst);
                gauge!("buffer.number", old_buffer_count + 1);
                Checkout { inner: c }
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
        gauge!("buffer.number", old_buffer_count - 1);
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
        self.inner.end = 0;
    }

    pub fn sync(&mut self, end: usize, position: usize) {
        self.inner.position = position;
        self.inner.end = end;
    }

    pub fn data(&self) -> &[u8] {
        &self.inner.extra()[self.inner.position..self.inner.end]
    }

    pub fn space(&mut self) -> &mut [u8] {
        let range = self.inner.end..self.capacity();
        &mut self.inner.extra_mut()[range]
    }

    pub fn shift(&mut self) {
        let pos = self.inner.position;
        let end = self.inner.end;
        if pos > 0 {
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
    }

    pub fn delete_slice(&mut self, start: usize, length: usize) -> Option<usize> {
        if start + length >= self.available_data() {
            return None;
        }

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
        Some(self.available_data())
    }

    pub fn replace_slice(&mut self, data: &[u8], start: usize, length: usize) -> Option<usize> {
        let data_len = data.len();
        if start + length > self.available_data()
            || self.inner.position + start + data_len > self.capacity()
        {
            return None;
        }

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
        Some(self.available_data())
    }

    pub fn insert_slice(&mut self, data: &[u8], start: usize) -> Option<usize> {
        let data_len = data.len();
        if start > self.available_data()
            || self.inner.position + self.inner.end + data_len > self.capacity()
        {
            return None;
        }

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
