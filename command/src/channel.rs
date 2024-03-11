use std::{
    cmp::min,
    fmt::Debug,
    io::{self, ErrorKind, Read, Write},
    marker::PhantomData,
    os::unix::{
        io::{AsRawFd, FromRawFd, IntoRawFd, RawFd},
        net::UnixStream as StdUnixStream,
    },
    time::Duration,
};

use mio::{event::Source, net::UnixStream as MioUnixStream};
use prost::{DecodeError, Message as ProstMessage};

use crate::{buffer::growable::Buffer, ready::Ready};

#[derive(thiserror::Error, Debug)]
pub enum ChannelError {
    #[error("io read error")]
    Read(std::io::Error),
    #[error("no byte written on the channel")]
    NoByteWritten,
    #[error("no byte left to read on the channel")]
    NoByteToRead,
    #[error("message too large for the capacity of the back fuffer ({0}. Consider increasing the back buffer size")]
    MessageTooLarge(usize),
    #[error("channel could not write on the back buffer")]
    Write(std::io::Error),
    #[error("channel buffer is full ({0} bytes), cannot grow more")]
    BufferFull(usize),
    #[error("Timeout is reached: {0:?}")]
    TimeoutReached(Duration),
    #[error("Could not read anything on the channel")]
    NothingRead,
    #[error("invalid char set in command message, ignoring: {0}")]
    InvalidCharSet(String),
    #[error("could not set the timeout of the unix stream with file descriptor {fd}: {error}")]
    SetTimeout { fd: i32, error: String },
    #[error("Could not change the blocking status ef the unix stream with file descriptor {fd}: {error}")]
    BlockingStatus { fd: i32, error: String },
    #[error("Connection error: {0:?}")]
    Connection(Option<std::io::Error>),
    #[error("Invalid protobuf message: {0}")]
    InvalidProtobufMessage(DecodeError),
    #[error("This should never happen (index out of bound on a tested buffer)")]
    MismatchBufferSize,
}

/// Channel meant for communication between Sōzu processes over a UNIX socket.
/// It wraps a unix socket using the mio crate, and transmit prost messages
/// by serializing them in a binary format, with a fix-sized delimiter.
/// To function, channels must come in pairs, one for each agent.
/// They can function in a blocking or non-blocking way.
pub struct Channel<Tx, Rx> {
    pub sock: MioUnixStream,
    pub front_buf: Buffer,
    pub back_buf: Buffer,
    max_buffer_size: u64,
    pub readiness: Ready,
    pub interest: Ready,
    blocking: bool,
    phantom_tx: PhantomData<Tx>,
    phantom_rx: PhantomData<Rx>,
}

impl<Tx, Rx> std::fmt::Debug for Channel<Tx, Rx> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct(&format!(
            "Channel<{}, {}>",
            std::any::type_name::<Tx>(),
            std::any::type_name::<Rx>()
        ))
        .field("sock", &self.sock.as_raw_fd())
        // .field("front_buf", &self.front_buf)
        // .field("back_buf", &self.back_buf)
        // .field("max_buffer_size", &self.max_buffer_size)
        .field("readiness", &self.readiness)
        .field("interest", &self.interest)
        .field("blocking", &self.blocking)
        .finish()
    }
}

impl<Tx: Debug + ProstMessage + Default, Rx: Debug + ProstMessage + Default> Channel<Tx, Rx> {
    /// Creates a nonblocking channel on a given socket path
    pub fn from_path(
        path: &str,
        buffer_size: u64,
        max_buffer_size: u64,
    ) -> Result<Channel<Tx, Rx>, ChannelError> {
        let unix_stream = MioUnixStream::connect(path)
            .map_err(|io_error| ChannelError::Connection(Some(io_error)))?;
        Ok(Channel::new(unix_stream, buffer_size, max_buffer_size))
    }

    /// Creates a nonblocking channel, using a unix stream
    pub fn new(sock: MioUnixStream, buffer_size: u64, max_buffer_size: u64) -> Channel<Tx, Rx> {
        Channel {
            sock,
            front_buf: Buffer::with_capacity(buffer_size as usize),
            back_buf: Buffer::with_capacity(buffer_size as usize),
            max_buffer_size,
            readiness: Ready::EMPTY,
            interest: Ready::READABLE,
            blocking: false,
            phantom_tx: PhantomData,
            phantom_rx: PhantomData,
        }
    }

    pub fn into<Tx2: Debug + ProstMessage + Default, Rx2: Debug + ProstMessage + Default>(
        self,
    ) -> Channel<Tx2, Rx2> {
        Channel {
            sock: self.sock,
            front_buf: self.front_buf,
            back_buf: self.back_buf,
            max_buffer_size: self.max_buffer_size,
            readiness: self.readiness,
            interest: self.interest,
            blocking: self.blocking,
            phantom_tx: PhantomData,
            phantom_rx: PhantomData,
        }
    }

    // Since MioUnixStream does not have a set_nonblocking method, we have to use the standard library.
    // We get the file descriptor of the MioUnixStream socket, create a standard library UnixStream,
    // set it to nonblocking, let go of the file descriptor
    fn set_nonblocking(&mut self, nonblocking: bool) -> Result<(), ChannelError> {
        unsafe {
            let fd = self.sock.as_raw_fd();
            let stream = StdUnixStream::from_raw_fd(fd);
            stream
                .set_nonblocking(nonblocking)
                .map_err(|error| ChannelError::BlockingStatus {
                    fd,
                    error: error.to_string(),
                })?;
            let _fd = stream.into_raw_fd();
        }
        self.blocking = !nonblocking;
        Ok(())
    }

    /// set the read_timeout of the unix stream. This works only temporary, be sure to set the timeout to None afterwards.
    fn set_timeout(&mut self, timeout: Option<Duration>) -> Result<(), ChannelError> {
        unsafe {
            let fd = self.sock.as_raw_fd();
            let stream = StdUnixStream::from_raw_fd(fd);
            stream
                .set_read_timeout(timeout)
                .map_err(|error| ChannelError::SetTimeout {
                    fd,
                    error: error.to_string(),
                })?;
            let _fd = stream.into_raw_fd();
        }
        Ok(())
    }

    /// set the channel to be blocking
    pub fn blocking(&mut self) -> Result<(), ChannelError> {
        self.set_nonblocking(false)
    }

    /// set the channel to be nonblocking
    pub fn nonblocking(&mut self) -> Result<(), ChannelError> {
        self.set_nonblocking(true)
    }

    pub fn is_blocking(&self) -> bool {
        self.blocking
    }

    /// Get the raw file descriptor of the UNIX socket
    pub fn fd(&self) -> RawFd {
        self.sock.as_raw_fd()
    }

    pub fn handle_events(&mut self, events: Ready) {
        self.readiness |= events;
    }

    pub fn readiness(&self) -> Ready {
        self.readiness & self.interest
    }

    /// Check wether we want and can read or write, and calls the appropriate handler.
    pub fn run(&mut self) -> Result<(), ChannelError> {
        let interest = self.interest & self.readiness;

        if interest.is_readable() {
            let _ = self.readable()?;
        }

        if interest.is_writable() {
            let _ = self.writable()?;
        }
        Ok(())
    }

    /// Handle readability by filling the front buffer with the socket data.
    pub fn readable(&mut self) -> Result<usize, ChannelError> {
        if !(self.interest & self.readiness).is_readable() {
            return Err(ChannelError::Connection(None));
        }

        let mut count = 0usize;
        loop {
            let size = self.front_buf.available_space();
            trace!("channel available space: {}", size);
            if size == 0 {
                self.interest.remove(Ready::READABLE);
                break;
            }

            match self.sock.read(self.front_buf.space()) {
                Ok(0) => {
                    self.interest = Ready::EMPTY;
                    self.readiness.remove(Ready::READABLE);
                    self.readiness.insert(Ready::HUP);
                    return Err(ChannelError::NoByteToRead);
                }
                Err(read_error) => match read_error.kind() {
                    ErrorKind::WouldBlock => {
                        self.readiness.remove(Ready::READABLE);
                        break;
                    }
                    _ => {
                        self.interest = Ready::EMPTY;
                        self.readiness = Ready::EMPTY;
                        return Err(ChannelError::Read(read_error));
                    }
                },
                Ok(bytes_read) => {
                    count += bytes_read;
                    self.front_buf.fill(bytes_read);
                }
            };
        }

        Ok(count)
    }

    /// Handle writability by writing the content of the back buffer onto the socket
    pub fn writable(&mut self) -> Result<usize, ChannelError> {
        if !(self.interest & self.readiness).is_writable() {
            return Err(ChannelError::Connection(None));
        }

        let mut count = 0usize;
        loop {
            let size = self.back_buf.available_data();
            if size == 0 {
                self.interest.remove(Ready::WRITABLE);
                break;
            }

            match self.sock.write(self.back_buf.data()) {
                Ok(0) => {
                    self.interest = Ready::EMPTY;
                    self.readiness.insert(Ready::HUP);
                    return Err(ChannelError::NoByteWritten);
                }
                Ok(bytes_written) => {
                    count += bytes_written;
                    self.back_buf.consume(bytes_written);
                }
                Err(write_error) => match write_error.kind() {
                    ErrorKind::WouldBlock => {
                        self.readiness.remove(Ready::WRITABLE);
                        break;
                    }
                    _ => {
                        self.interest = Ready::EMPTY;
                        self.readiness = Ready::EMPTY;
                        return Err(ChannelError::Read(write_error));
                    }
                },
            }
        }

        Ok(count)
    }

    /// Depending on the blocking status:
    ///
    /// Blocking: wait for the front buffer to be filled, and parse a message from it
    ///
    /// Nonblocking: parse a message from the front buffer, without waiting.
    /// Prefer using `channel.readable()` before
    pub fn read_message(&mut self) -> Result<Rx, ChannelError> {
        if self.blocking {
            self.read_message_blocking()
        } else {
            self.read_message_nonblocking()
        }
    }

    fn read_message_blocking(&mut self) -> Result<Rx, ChannelError> {
        self.read_message_blocking_timeout(None)
    }

    /// Parse a message from the front buffer, without waiting
    fn read_message_nonblocking(&mut self) -> Result<Rx, ChannelError> {
        if let Some(message) = self.try_read_delimited_message()? {
            return Ok(message);
        }

        self.interest.insert(Ready::READABLE);
        Err(ChannelError::NothingRead)
    }

    /// Wait for the front buffer to be filled, and parses a message from it.
    pub fn read_message_blocking_timeout(
        &mut self,
        timeout: Option<Duration>,
    ) -> Result<Rx, ChannelError> {
        let now = std::time::Instant::now();

        // set a very small timeout, to repeat the loop often
        self.set_timeout(Some(Duration::from_millis(10)))?;

        let status = loop {
            if let Some(timeout) = timeout {
                if now.elapsed() >= timeout {
                    break Err(ChannelError::TimeoutReached(timeout));
                }
            }

            if let Some(message) = self.try_read_delimited_message()? {
                return Ok(message);
            }

            match self.sock.read(self.front_buf.space()) {
                Ok(0) => return Err(ChannelError::NoByteToRead),
                Ok(bytes_read) => self.front_buf.fill(bytes_read),
                Err(io_error) => match io_error.kind() {
                    ErrorKind::WouldBlock => continue, // ignore 10 millisecond timeouts
                    _ => break Err(ChannelError::Read(io_error)),
                },
            };
        };

        self.set_timeout(None)?;

        status
    }

    /// parse a prost message from the front buffer, grow it if necessary
    fn try_read_delimited_message(&mut self) -> Result<Option<Rx>, ChannelError> {
        let buffer = self.front_buf.data();
        if buffer.len() >= delimiter_size() {
            let delimiter = buffer[..delimiter_size()]
                .try_into()
                .map_err(|_| ChannelError::MismatchBufferSize)?;
            let message_len = usize::from_le_bytes(delimiter);

            if buffer.len() >= message_len {
                let message = Rx::decode(&buffer[delimiter_size()..message_len])
                    .map_err(ChannelError::InvalidProtobufMessage)?;
                self.front_buf.consume(message_len);
                return Ok(Some(message));
            }
        }

        if self.front_buf.available_space() == 0 {
            if (self.front_buf.capacity() as u64) >= self.max_buffer_size {
                return Err(ChannelError::BufferFull(self.front_buf.capacity()));
            }
            let new_size = min(
                self.front_buf.capacity() + 5000,
                self.max_buffer_size as usize,
            );
            self.front_buf.grow(new_size);
        }
        Ok(None)
    }

    /// Checks whether the channel is blocking or nonblocking, writes the message.
    ///
    /// If the channel is nonblocking, you have to flush using `channel.run()` afterwards
    pub fn write_message(&mut self, message: &Tx) -> Result<(), ChannelError> {
        if self.blocking {
            self.write_message_blocking(message)
        } else {
            self.write_message_nonblocking(message)
        }
    }

    /// Writes the message in the buffer, but NOT on the socket.
    /// you have to call channel.run() afterwards
    fn write_message_nonblocking(&mut self, message: &Tx) -> Result<(), ChannelError> {
        self.write_delimited_message(message)?;

        self.interest.insert(Ready::WRITABLE);

        Ok(())
    }

    /// fills the back buffer with data AND writes on the socket
    fn write_message_blocking(&mut self, message: &Tx) -> Result<(), ChannelError> {
        self.write_delimited_message(message)?;

        loop {
            let size = self.back_buf.available_data();
            if size == 0 {
                break;
            }

            match self.sock.write(self.back_buf.data()) {
                Ok(0) => return Err(ChannelError::NoByteWritten),
                Ok(bytes_written) => {
                    self.back_buf.consume(bytes_written);
                }
                Err(_) => return Ok(()), // are we sure?
            }
        }
        Ok(())
    }

    /// write a message on the back buffer, using our own delimiter (the delimiter of prost
    /// is not trustworthy since its size may change)
    pub fn write_delimited_message(&mut self, message: &Tx) -> Result<(), ChannelError> {
        let payload = message.encode_to_vec();

        let payload_len = payload.len() + delimiter_size();

        let delimiter = payload_len.to_le_bytes();

        if payload_len > self.back_buf.available_space() {
            self.back_buf.shift();
        }

        if payload_len > self.back_buf.available_space() {
            if payload_len - self.back_buf.available_space() + self.back_buf.capacity()
                > (self.max_buffer_size as usize)
            {
                return Err(ChannelError::MessageTooLarge(self.back_buf.capacity()));
            }

            let new_length =
                payload_len - self.back_buf.available_space() + self.back_buf.capacity();
            self.back_buf.grow(new_length);
        }

        self.back_buf
            .write_all(&delimiter)
            .map_err(ChannelError::Write)?;
        self.back_buf
            .write_all(&payload)
            .map_err(ChannelError::Write)?;

        Ok(())
    }
}

/// the payload is prefixed with a delimiter of sizeof(usize) bytes
pub const fn delimiter_size() -> usize {
    std::mem::size_of::<usize>()
}

type ChannelResult<Tx, Rx> = Result<(Channel<Tx, Rx>, Channel<Rx, Tx>), ChannelError>;

impl<Tx: Debug + ProstMessage + Default, Rx: Debug + ProstMessage + Default> Channel<Tx, Rx> {
    /// creates a channel pair: `(blocking_channel, nonblocking_channel)`
    pub fn generate(buffer_size: u64, max_buffer_size: u64) -> ChannelResult<Tx, Rx> {
        let (command, proxy) = MioUnixStream::pair().map_err(ChannelError::Read)?;
        let proxy_channel = Channel::new(proxy, buffer_size, max_buffer_size);
        let mut command_channel = Channel::new(command, buffer_size, max_buffer_size);
        command_channel.blocking()?;
        Ok((command_channel, proxy_channel))
    }

    /// creates a pair of nonblocking channels
    pub fn generate_nonblocking(buffer_size: u64, max_buffer_size: u64) -> ChannelResult<Tx, Rx> {
        let (command, proxy) = MioUnixStream::pair().map_err(ChannelError::Read)?;
        let proxy_channel = Channel::new(proxy, buffer_size, max_buffer_size);
        let command_channel = Channel::new(command, buffer_size, max_buffer_size);
        Ok((command_channel, proxy_channel))
    }
}

impl<Tx: Debug + ProstMessage + Default, Rx: Debug + ProstMessage + Default> Iterator
    for Channel<Tx, Rx>
{
    type Item = Rx;
    fn next(&mut self) -> Option<Self::Item> {
        self.read_message().ok()
    }
}

use mio::{Interest, Registry, Token};
impl<Tx, Rx> Source for Channel<Tx, Rx> {
    fn register(
        &mut self,
        registry: &Registry,
        token: Token,
        interests: Interest,
    ) -> io::Result<()> {
        self.sock.register(registry, token, interests)
    }

    fn reregister(
        &mut self,
        registry: &Registry,
        token: Token,
        interests: Interest,
    ) -> io::Result<()> {
        self.sock.reregister(registry, token, interests)
    }

    fn deregister(&mut self, registry: &Registry) -> io::Result<()> {
        self.sock.deregister(registry)
    }
}

#[cfg(test)]
mod tests {
    use std::{thread, time::Duration};

    use super::*;

    #[derive(Clone, PartialEq, prost::Message)]
    pub struct ProtobufMessage {
        #[prost(uint32, required, tag = "1")]
        inner: u32,
    }

    fn test_channels() -> (
        Channel<ProtobufMessage, ProtobufMessage>,
        Channel<ProtobufMessage, ProtobufMessage>,
    ) {
        Channel::generate(1000, 10000).expect("could not generate blocking channels for testing")
    }

    #[test]
    fn unblock_a_channel() {
        let (mut blocking, _nonblocking) = test_channels();
        assert!(blocking.nonblocking().is_ok())
    }

    #[test]
    fn generate_blocking_and_nonblocking_channels() {
        let (blocking_channel, nonblocking_channel) = test_channels();

        assert!(blocking_channel.is_blocking());
        assert!(!nonblocking_channel.is_blocking());

        let (nonblocking_channel_1, nonblocking_channel_2): (
            Channel<ProtobufMessage, ProtobufMessage>,
            Channel<ProtobufMessage, ProtobufMessage>,
        ) = Channel::generate_nonblocking(1000, 10000)
            .expect("could not generatie nonblocking channels");

        assert!(!nonblocking_channel_1.is_blocking());
        assert!(!nonblocking_channel_2.is_blocking());
    }

    #[test]
    fn write_and_read_message_blocking() {
        let (mut blocking_channel, mut nonblocking_channel) = test_channels();

        let message_to_send = ProtobufMessage { inner: 42 };

        nonblocking_channel
            .blocking()
            .expect("Could not block channel");
        nonblocking_channel
            .write_message(&message_to_send)
            .expect("Could not write message on channel");

        trace!("we wrote a message!");

        trace!("reading message..");
        // blocking_channel.readable();
        let message = blocking_channel
            .read_message()
            .expect("Could not read message on channel");
        trace!("read message!");

        assert_eq!(message, ProtobufMessage { inner: 42 });
    }

    #[test]
    fn read_message_blocking_with_timeout_fails() {
        let (mut reading_channel, mut writing_channel) = test_channels();
        writing_channel.blocking().expect("Could not block channel");

        trace!("reading message in a detached thread, with a timeout of 100 milliseconds...");
        let awaiting_with_timeout = thread::spawn(move || {
            let message =
                reading_channel.read_message_blocking_timeout(Some(Duration::from_millis(100)));
            trace!("read message!");
            message
        });

        trace!("Waiting 200 milliseconds…");
        thread::sleep(std::time::Duration::from_millis(200));

        writing_channel
            .write_message(&ProtobufMessage { inner: 200 })
            .expect("Could not write message on channel");
        trace!("we wrote a message that should arrive too late!");

        let arrived_too_late = awaiting_with_timeout
            .join()
            .expect("error with receiving message from awaiting thread");

        assert!(arrived_too_late.is_err());
    }

    #[test]
    fn read_message_blocking_with_timeout_succeeds() {
        let (mut reading_channel, mut writing_channel) = test_channels();
        writing_channel.blocking().expect("Could not block channel");

        trace!("reading message in a detached thread, with a timeout of 200 milliseconds...");
        let awaiting_with_timeout = thread::spawn(move || {
            let message = reading_channel
                .read_message_blocking_timeout(Some(Duration::from_millis(200)))
                .expect("Could not read message with timeout on blocking channel");
            trace!("read message!");
            message
        });

        trace!("Waiting 100 milliseconds…");
        thread::sleep(std::time::Duration::from_millis(100));

        writing_channel
            .write_message(&ProtobufMessage { inner: 100 })
            .expect("Could not write message on channel");
        trace!("we wrote a message that should arrive on time!");

        let arrived_on_time = awaiting_with_timeout
            .join()
            .expect("error with receiving message from awaiting thread");

        assert_eq!(arrived_on_time, ProtobufMessage { inner: 100 });
    }

    #[test]
    fn exhaustive_use_of_nonblocking_channels() {
        // - two nonblocking channels A and B, identical
        let (mut channel_a, mut channel_b) = test_channels();
        channel_a.nonblocking().expect("Could not block channel");

        // write on A
        channel_a
            .write_message(&ProtobufMessage { inner: 1 })
            .expect("Could not write message on channel");

        // set B as readable, normally mio tells when to, by giving events
        channel_b.handle_events(Ready::READABLE);

        // read on B
        let should_err = channel_b.read_message();
        assert!(should_err.is_err());

        // write another message on A
        channel_a
            .write_message(&ProtobufMessage { inner: 2 })
            .expect("Could not write message on channel");

        // insert a handle_events Ready::writable on A
        channel_a.handle_events(Ready::WRITABLE);

        // flush A with run()
        channel_a.run().expect("Failed to run the channel");

        // maybe a thread sleep
        thread::sleep(std::time::Duration::from_millis(100));

        // receive with B using run()
        channel_b.run().expect("Failed to run the channel");

        // use read_message() twice on B, check them
        let message_1 = channel_b
            .read_message()
            .expect("Could not read message on channel");
        assert_eq!(message_1, ProtobufMessage { inner: 1 });

        let message_2 = channel_b
            .read_message()
            .expect("Could not read message on channel");
        assert_eq!(message_2, ProtobufMessage { inner: 2 });
    }
}
