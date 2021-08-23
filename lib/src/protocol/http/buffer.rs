use crate::pool::{Checkout, Pool};
use crate::pool_crate::Reset;
use std::{fmt, io};

pub struct HttpBuffer {
    // position in the stream of the first byte of the buffer
    pub buffer_position: usize,
    // position in the stream of the last parsed and consumed byte
    pub parsed_position: usize,
    buffer: Checkout,
}

impl HttpBuffer {
    pub fn with_buffer(buffer: Checkout) -> HttpBuffer {
        HttpBuffer {
            buffer_position: 0,
            parsed_position: 0,
            buffer,
        }
    }

    pub fn into_inner(self) -> Checkout {
        self.buffer
    }

    pub fn invariant(&self) {
        debug_assert!(
            self.buffer_position <= self.parsed_position,
            "buffer_position {} should be smaller than parsed_position {}",
            self.buffer_position,
            self.parsed_position
        );
        /*debug_assert!(self.parsed_position <= self.start_parsing_position,
        "parsed_position {} should be smaller than start_parsing_position {}",
        self.parsed_position, self.start_parsing_position);*/
    }

    pub fn available_data(&self) -> usize {
        self.unparsed_data().len()
    }

    pub fn available_space(&self) -> usize {
        self.buffer.available_space()
    }

    pub fn space(&mut self) -> &mut [u8] {
        self.buffer.space()
    }

    pub fn fill(&mut self, sz: usize) {
        self.buffer.fill(sz);
    }

    pub fn unparsed_data(&self) -> &[u8] {
        let start = std::cmp::min(
            self.buffer.available_data(),
            self.parsed_position - self.buffer_position,
        );

        &self.buffer.data()[start..]
    }

    pub fn consume_parsed_data(&mut self, size: usize) {
        self.parsed_position += size;

        if self.parsed_position - self.buffer_position > self.buffer.available_data() / 2 {
            let sz = self
                .buffer
                .consume(self.parsed_position - self.buffer_position);
            self.buffer_position += sz;
        }
    }

    pub fn consume_up_to(&mut self, size: usize) {
        self.parsed_position = size;

        if self.parsed_position - self.buffer_position > self.buffer.available_data() / 2 {
            let sz = self
                .buffer
                .consume(self.parsed_position - self.buffer_position);
            self.buffer_position += sz;
        }
    }

    pub fn as_ioslice(&self) -> Vec<std::io::IoSlice> {
        /*let mut res = Vec::new();

        let it = self.output_queue.iter();
        //first, calculate how many bytes we need to jump
        let mut start         = 0usize;
        let mut largest_size  = 0usize;
        let mut delete_ended  = false;
        let length = self.buffer.available_data();
        //println!("NEXT OUTPUT DATA:\nqueue:\n{:?}\nbuffer:\n{}", self.output_queue, self.buffer.data().to_hex(16));
        let mut complete_size = 0;
        for el in it {
          match el {
            &OutputElement::Delete(sz) => start += sz,
            &OutputElement::Slice(sz)  => {
              //println!("Slice({})", sz);
              if sz == 0 {
                continue
              }
              let end = min(start+sz, length);
              let i = std::io::IoSlice::new(&self.buffer.data()[start..end]);
              //println!("iovec size: {}", i.len());
              res.push(i);
              complete_size += i.len();
              start = end;
              if end == length {
                break;
              }
            }
            &OutputElement::Insert(ref v) => {
              if v.is_empty() {
                continue
              }
              let i = std::io::IoSlice::new(&v[..]);
              //println!("got Insert with {} bytes", v.len());
              res.push(i);
              complete_size += i.len();
            },
            &OutputElement::Splice(sz)  => { unimplemented!("splice not used in ioslice") },
          }
        }

        //println!("returning iovec: {:?}", res);
        //println!("returning iovec with {} bytes", complete_size);
        res*/
        unimplemented!()
    }
}

impl io::Write for HttpBuffer {
    fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
        match self.buffer.write(buf) {
            Err(e) => Err(e),
            Ok(sz) => {
                /*if sz > 0 {
                  self.input_queue.push(InputElement::Slice(sz));
                }*/
                Ok(sz)
            }
        }
    }

    fn flush(&mut self) -> io::Result<()> {
        Ok(())
    }
}
impl Reset for HttpBuffer {
    fn reset(&mut self) {
        self.parsed_position = 0;
        self.buffer_position = 0;
        self.buffer.reset();
    }
}

impl fmt::Debug for HttpBuffer {
    fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
        //let b: &Buffer = &self.buffer;
        write!(
            f,
            "BufferQueue {{\nbuffer_position: {},\nparsed_position: {},\nbuffer: {:?}\n}}",
            self.buffer_position,
            self.parsed_position,
            /*b*/ ()
        )
    }
}

pub fn http_buf_with_capacity(capacity: usize) -> (Pool, HttpBuffer) {
    let mut pool = Pool::with_capacity(1, capacity, 16384);
    let b = HttpBuffer::with_buffer(pool.checkout().unwrap());
    (pool, b)
}
