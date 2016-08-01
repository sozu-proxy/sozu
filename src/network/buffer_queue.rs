use network::buffer::Buffer;

#[derive(Debug,PartialEq)]
pub enum Element {
  /// start, end in the stream
  Slice(usize, usize),
  Insert(Vec<u8>),
  // Splice(usize) // should copy x bytes in kernel
}

impl Element {
  pub fn consume(&self, size: usize) -> (usize,Option<Element>) {
    println!("consuming {} bytes of {:?}", size, self);
    match *self {
      Element::Slice(begin, end) => {
        if size >= end - begin {
          (size - (end - begin), None)
        } else {
          (0, Some(Element::Slice(begin + size, end)))
        }
      },
      Element::Insert(ref v)     => {
        if size >= v.len() {
          (size - v.len(), None)
        } else {
          (0, Some(Element::Insert(Vec::from(&v[size..]))))
        }
      }
    }
  }

  pub fn available_data(&self) -> usize {
    match self {
      &Element::Slice(begin, end) => end - begin,
      &Element::Insert(ref data)  => data.len(),
    }
  }
}

/// The BufferQueue has two roles: holding incoming data, and indicating
/// which data will go out. When new data arrives, it is added at the
/// end of the internal buffer. This new data is then eventually parsed or
/// handled in some way by external code. The external code then adds
/// element to the queue, indicating what to do with the data:
///   - copy a subset of the input data (and advance if needed)
///   - insert external data, like a HTTP header
///   - splice out of the kernel some data that was spliced in
///
/// position is the index in the stream of data already handled.
/// it corresponds to the beginning of available data in the Buffer
/// a Slice(begin, end) would point to buffer.data()[begin-position..end-position]
/// (in the easiest case)
///
/// The buffer's available data may be smaller than `end - begin`.
/// It can happen if the parser indicated we need to copy more data than is available,
/// like with a content length
///
/// should the buffer queue indicate how much data it needs?
#[derive(Debug,PartialEq)]
pub struct BufferQueue {
  pub position:  usize,
  pub buffer:    Buffer,
  pub queue:     Vec<Element>,
}

impl BufferQueue {
  pub fn available_data(&self) -> usize {
    self.queue.iter().fold(0, |acc, ref el| acc + el.available_data())
  }

  //FIXME: implement this
  pub fn unparsed_data(&self) {}
  //FIXME: implement this
  pub fn consumed_unparsed_data(&mut self) {}
  //FIXME: implement this
  pub fn needed_data(&self)  {}

  pub fn next_buffer(&self) -> Option<&[u8]> {
    self.queue.get(0).map(|el| {
      match el {
        &Element::Slice(begin, end) => self.calculate_slice(begin, end),
        &Element::Insert(ref v)     => &v[..]
      }
    })
  }

  /// begin should not be smaller than position
  /// begin can be larger than position+buffer_available_size
  /// end must be larger than begin
  /// bufferqueue.position => position in the stream (may be larger than the underlying buffer)
  /// buffer.position      => position in the buffer (no larger than buffer.capacity)
  /// buffer.end           => end of available data in the buffer (no larger than buffer.capacity)
  /// slice.begin          => position of slice beginning in the stream (larger or equal to bufferqueue.position,
  ///                           can be larger than buffer position, end or capacity)
  /// slice.end            => position of slice end in the stream (larger or equal to slice.begin,
  ///                           can be larger than buffer position, end or capacity)
  ///
  ///  position in input stream or output stream (where do we count the insert?)
  pub fn calculate_slice(&self, begin:usize, end: usize) -> &[u8] {
    &self.buffer.data()[(begin - self.position)..(end - self.position)]
  }

  pub fn consume(&mut self, size: usize) {
    let mut to_consume = size;
    let mut consumed   = 0;
    while to_consume > 0 {
      let new_first_element = match self.queue.first() {
        None => {
          assert!(to_consume == 0, "no more element in queue, we should not ask to consume {} more bytes", to_consume);
          break;
        },
        Some(el) => match el {
          &Element::Slice(begin, end) => {
            if to_consume >= end - begin {
              consumed  += end - begin;
              to_consume = to_consume - (end - begin);
              None
            } else {
              consumed += to_consume;
              let new_element = Element::Slice(begin + to_consume, end);
              to_consume = 0;
              Some(new_element)
            }
          },
          &Element::Insert(ref v)     => {
            if to_consume >= v.len() {
              to_consume = to_consume - v.len();
              None
            } else {
              let new_element = Element::Insert(Vec::from(&v[to_consume..]));
              to_consume = 0;
              Some(new_element)
            }
          }
        }
      };

      match new_first_element {
        None     => { self.queue.remove(0); },
        // FIXME: do we test for the presence of the first element here?
        Some(el) => { self.queue[0] = el; },
      };
    };

    println!("consumed: {}", consumed);
    self.buffer.consume(consumed);
    self.position += consumed;
  }

  pub fn next(&self) -> Option<&Element> {
    self.queue.get(0)
  }

  // FIXME: implement Write to get incoming data
  pub fn write(&mut self, input: &[u8]) {}
}

#[cfg(test)]
mod tests {
  use super::*;
  use network::buffer::Buffer;
  use nom::HexDisplay;

  #[test]
  fn consume() {
    let mut b = BufferQueue {
      position: 0,
      buffer:   Buffer::from_slice(b"ABCDEFGHIJKLMNOPQRSTUVWXYZ"),
      queue:    vec!(
        Element::Slice(0, 3),
        Element::Insert(Vec::from(&b"test"[..])),
        Element::Slice(5, 10)
      )
    };

    assert_eq!(b.next_buffer(), Some(&b"ABC"[..]));
    b.consume(3);
    assert_eq!(b.position, 3);
    assert_eq!(b.queue, vec!(
        Element::Insert(Vec::from(&b"test"[..])),
        Element::Slice(5, 10))
    );

    assert_eq!(b.next_buffer(), Some(&b"test"[..]));
    b.consume(2);
    assert_eq!(b.position, 3);
    assert_eq!(b.queue, vec!(
        Element::Insert(Vec::from(&b"st"[..])),
        Element::Slice(5, 10))
    );

    assert_eq!(b.next_buffer(), Some(&b"st"[..]));
    b.consume(2);
    assert_eq!(b.position, 3);
    assert_eq!(b.queue, vec!(
        Element::Slice(5, 10))
    );

    assert_eq!(b.next_buffer(), Some(&b"FGHIJ"[..]));
    b.consume(2);
    assert_eq!(b.position, 5);
    assert_eq!(b.queue, vec!(Element::Slice(7, 10)));

    assert_eq!(b.next_buffer(), Some(&b"HIJ"[..]));
    b.consume(3);
    assert_eq!(b.position, 8);
    assert_eq!(b.queue, vec!());

    assert_eq!(b.next_buffer(), None);
  }
}
