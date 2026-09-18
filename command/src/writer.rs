use std::io::{self, Error, ErrorKind, Write};

/// A multiline writer used for logging
pub struct MultiLineWriter<W: Write> {
    inner: Option<W>,
    buf: Vec<u8>,
    last_newline: usize,
    panicked: bool,
}

impl<W: Write> MultiLineWriter<W> {
    pub fn new(inner: W) -> MultiLineWriter<W> {
        MultiLineWriter::with_capacity(4096, inner)
    }

    pub fn with_capacity(capacity: usize, inner: W) -> MultiLineWriter<W> {
        //MultiLineWriter { inner: BufWriter::with_capacity(capacity, inner), capacity, last_newline: usize, need_flush: false }
        MultiLineWriter {
            inner: Some(inner),
            buf: Vec::with_capacity(capacity),
            panicked: false,
            last_newline: 0,
        }
    }

    pub fn get_ref(&self) -> &W {
        self.inner.as_ref().unwrap()
    }

    pub fn get_mut(&mut self) -> &mut W {
        self.inner.as_mut().unwrap()
    }

    fn flush_buf(&mut self, flush_entire_buffer: bool) -> io::Result<()> {
        let mut written = 0;
        // `last_newline` is stale whenever the buffer is empty: `flush_buf`
        // resets it to 0, which is also a valid index, so a partial flush of an
        // EMPTY buffer used to slice `..1` out of a zero-length `Vec` and panic
        // in release. `write` takes exactly that path for a record larger than
        // the capacity, and access-log records are network-influenced.
        let len = if flush_entire_buffer {
            self.buf.len()
        } else {
            (self.last_newline + 1).min(self.buf.len())
        };

        let mut ret = Ok(());
        while written < len {
            self.panicked = true;
            let r = self.inner.as_mut().unwrap().write(&self.buf[written..len]);
            self.panicked = false;

            match r {
                Ok(0) => {
                    ret = Err(Error::new(
                        ErrorKind::WriteZero,
                        "failed to write the buffered data",
                    ));
                    break;
                }
                Ok(n) => written += n,
                Err(ref e) if e.kind() == io::ErrorKind::Interrupted => {}
                Err(e) => {
                    ret = Err(e);
                    break;
                }
            }
        }
        //println!("buf len {} last newline {}, written {}", self.buf.len(), self.last_newline, written);
        if written > 0 {
            //println!("FLUSHED: {}", ::std::str::from_utf8(&self.buf[..written]).unwrap());
            self.buf.drain(..written);
        }

        if flush_entire_buffer {
            self.last_newline = 0;
        } else if written > self.last_newline {
            self.last_newline = 0
        } else {
            self.last_newline -= written;
        }

        ret
    }
}

impl<W: Write> Write for MultiLineWriter<W> {
    fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
        if self.buf.len() + buf.len() > self.buf.capacity() {
            self.flush_buf(false)?;
        }
        if buf.len() >= self.buf.capacity() {
            self.panicked = true;
            let r = self.get_mut().write(buf);
            self.panicked = false;
            r
        } else {
            if let Some(i) = memchr::memrchr(b'\n', buf) {
                self.last_newline = self.buf.len() + i;
            };

            self.buf.write(buf)
        }
    }

    fn flush(&mut self) -> io::Result<()> {
        self.flush_buf(true).and_then(|()| self.get_mut().flush())
    }
}

impl<W: Write> Drop for MultiLineWriter<W> {
    fn drop(&mut self) {
        if self.inner.is_some() && !self.panicked {
            // dtors should not panic, so we ignore a failed flush
            let _r = self.flush_buf(true);
        }
    }
}

/*
impl<W: Write> Write for MultiLineWriter<W> {
  fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
    let buffer_len = self.inner.buffer().len();

    let i = match memchr::memrchr(b'\n', buf) {
      Some(i) => i,
      None => buf.len(),
    };

    if buffer_len + i > self.capacity {
      self.inner.flush()?;
      self.last_newline = 0;
    }




  }
}
*/

/*
impl<W: Write> Drop for BufWriter<W> {
  fn drop(&mut self) {
    if self.inner.is_some() && !self.panicked {
      // dtors should not panic, so we ignore a failed flush
      let _r = self.flush_buf();
    }
  }
}
*/

#[cfg(test)]
mod tests {
    //! `MultiLineWriter::write` must never index past its own buffer. The
    //! `file://` logger backend (`command/src/logging/logs.rs`) and
    //! `MetricsWriter` (`lib/src/metrics/writer.rs`) both wrap one of these
    //! around network-influenced records, so a record larger than the 4096-byte
    //! capacity is reachable from traffic — and it panics in RELEASE, where a
    //! slice index is not a `debug_assert!`.
    //!
    //! To SEE THESE RED (regression proof): restore the pre-fix body of
    //! [`super::MultiLineWriter::flush_buf`]'s partial branch, `self.last_newline + 1`
    //! without the `.min(self.buf.len())` clamp — the two oversized-record
    //! expectations below then panic with
    //! `range end index 1 out of range for slice of length 0`.
    use super::MultiLineWriter;
    use std::io::Write;

    /// Capacity of the writer under test. The production default is 4096
    /// (`MultiLineWriter::new`); the bug is a function of `record > capacity`,
    /// not of the capacity's value, so a small one keeps the fixtures readable.
    const CAPACITY: usize = 16;

    #[test]
    fn an_oversized_record_on_a_fresh_writer_does_not_panic() {
        // `last_newline == 0` with an EMPTY buffer is the initial state: the
        // partial flush computed `len = 1` against a zero-length `Vec`.
        let mut writer = MultiLineWriter::with_capacity(CAPACITY, Vec::new());
        let record = vec![b'a'; CAPACITY * 4];

        let written = writer
            .write(&record)
            .expect("a record larger than the capacity is written straight through");

        assert_eq!(
            written,
            record.len(),
            "the oversized record must be handed to the inner writer whole"
        );
        assert_eq!(
            writer.get_ref(),
            &record,
            "the inner writer must receive exactly the oversized record"
        );
    }

    #[test]
    fn an_oversized_record_after_a_flush_does_not_panic() {
        // The same empty-buffer/`last_newline == 0` state, reached the way a
        // live logger reaches it: any previous `flush()` leaves it behind.
        let mut writer = MultiLineWriter::with_capacity(CAPACITY, Vec::new());
        writer.write_all(b"x\n").expect("a short line is buffered");
        writer.flush().expect("the flush empties the buffer");
        assert_eq!(
            writer.get_ref(),
            b"x\n",
            "the short line reached the inner writer"
        );

        let record = vec![b'b'; CAPACITY * 4];
        let written = writer
            .write(&record)
            .expect("a record larger than the capacity is written straight through");

        assert_eq!(written, record.len());
        assert_eq!(
            &writer.get_ref()[2..],
            &record[..],
            "the oversized record must follow the already-flushed line"
        );
    }

    #[test]
    fn a_partial_flush_still_stops_at_the_last_newline() {
        // The clamp must not change the path it was NOT meant to touch: with a
        // non-empty buffer, a partial flush still writes up to and including
        // the last newline and keeps the unterminated tail buffered.
        let mut writer = MultiLineWriter::with_capacity(CAPACITY, Vec::new());
        writer
            .write_all(b"aaaa\nbb")
            .expect("buffered below capacity");
        assert!(
            writer.get_ref().is_empty(),
            "nothing reaches the inner writer before a flush is triggered"
        );

        // 7 + 10 > 16 triggers the partial flush of the completed line only.
        writer
            .write_all(b"cccccccccc")
            .expect("triggers a partial flush");
        assert_eq!(
            writer.get_ref(),
            b"aaaa\n",
            "a partial flush writes up to and including the last newline"
        );

        writer.flush().expect("the final flush drains the tail");
        assert_eq!(
            writer.get_ref(),
            b"aaaa\nbbcccccccccc",
            "the buffered tail must survive the partial flush"
        );
    }
}
