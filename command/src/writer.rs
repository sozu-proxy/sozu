use std::io::{self, Error, ErrorKind, Write};

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
        let len = if flush_entire_buffer {
            self.buf.len()
        } else {
            self.last_newline + 1
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
