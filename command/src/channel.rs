use std::{
    cmp::min,
    fmt::Debug,
    io::{self, ErrorKind, Read, Write},
    iter::Iterator,
    marker::PhantomData,
    os::unix::{
        io::{AsRawFd, FromRawFd, IntoRawFd, RawFd},
        net::UnixStream as StdUnixStream,
    },
    str::from_utf8,
    time::Duration,
};

use mio::{event::Source, net::UnixStream as MioUnixStream};
use serde::{de::DeserializeOwned, ser::Serialize};
use serde_json;

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
    #[error("channel buffer is full, cannot grow more")]
    BufferFull,
    #[error("Timeout is reached: {0:?}")]
    TimeoutReached(Duration),
    #[error("Could not read anything on the channel")]
    NothingRead,
    #[error("invalid char set in command message, ignoring: {0}")]
    InvalidCharSet(String),
    #[error("Error deserializing message")]
    Serde(serde_json::error::Error),
    #[error("Could not change the blocking status ef the unix stream with file descriptor {fd}: {error}")]
    BlockingStatus { fd: i32, error: String },
    #[error("Connection error: {0:?}")]
    Connection(Option<std::io::Error>),
}

/// A wrapper around unix socket using the mio crate.
/// Used in pairs to communicate, in a blocking or non-blocking way.
pub struct Channel<Tx, Rx> {
    pub sock: MioUnixStream,
    front_buf: Buffer,
    pub back_buf: Buffer,
    max_buffer_size: u64,
    pub readiness: Ready,
    pub interest: Ready,
    blocking: bool,
    phantom_tx: PhantomData<Tx>,
    phantom_rx: PhantomData<Rx>,
}

impl<Tx: Debug + Serialize, Rx: Debug + DeserializeOwned> Channel<Tx, Rx> {
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

    pub fn into<Tx2: Debug + Serialize, Rx2: Debug + DeserializeOwned>(self) -> Channel<Tx2, Rx2> {
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

    pub fn fd(&self) -> RawFd {
        self.sock.as_raw_fd()
    }

    pub fn handle_events(&mut self, events: Ready) {
        self.readiness |= events;
    }

    pub fn readiness(&self) -> Ready {
        self.readiness & self.interest
    }

    /// Checks wether we want and can read or write, and calls the appropriate handler.
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

    /// Handles readability by filling the front buffer with the socket data.
    pub fn readable(&mut self) -> Result<usize, ChannelError> {
        if !(self.interest & self.readiness).is_readable() {
            return Err(ChannelError::Connection(None));
        }

        let mut count = 0usize;
        loop {
            let size = self.front_buf.available_space();
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

    /// Handles writability by writing the content of the back buffer onto the socket
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
    /// Blocking: waits for the front buffer to be filled, and parses a message from it
    ///
    /// Nonblocking: parses a message from the front buffer, without waiting.
    /// Prefer using `channel.readable()` before
    pub fn read_message(&mut self) -> Result<Rx, ChannelError> {
        if self.blocking {
            self.read_message_blocking()
        } else {
            self.read_message_nonblocking()
        }
    }

    /// Parses a message from the front buffer, without waiting
    fn read_message_nonblocking(&mut self) -> Result<Rx, ChannelError> {
        match self.front_buf.data().iter().position(|&x| x == 0) {
            Some(position) => self.read_and_parse_from_front_buffer(position),
            None => {
                if self.front_buf.available_space() == 0 {
                    if (self.front_buf.capacity() as u64) == self.max_buffer_size {
                        error!("command buffer full, cannot grow more, ignoring");
                    } else {
                        let new_size = min(
                            self.front_buf.capacity() + 5000,
                            self.max_buffer_size as usize,
                        );
                        self.front_buf.grow(new_size);
                    }
                }

                self.interest.insert(Ready::READABLE);
                Err(ChannelError::NothingRead)
            }
        }
    }

    fn read_message_blocking(&mut self) -> Result<Rx, ChannelError> {
        self.read_message_blocking_timeout(None)
    }

    /// Waits for the front buffer to be filled, and parses a message from it.
    pub fn read_message_blocking_timeout(
        &mut self,
        timeout: Option<Duration>,
    ) -> Result<Rx, ChannelError> {
        let now = std::time::Instant::now();

        loop {
            if let Some(timeout) = timeout {
                if now.elapsed() >= timeout {
                    return Err(ChannelError::TimeoutReached(timeout));
                }
            }

            match self.front_buf.data().iter().position(|&x| x == 0) {
                Some(position) => return self.read_and_parse_from_front_buffer(position),
                None => {
                    if self.front_buf.available_space() == 0 {
                        if (self.front_buf.capacity() as u64) == self.max_buffer_size {
                            return Err(ChannelError::BufferFull);
                        }
                        let new_size = min(
                            self.front_buf.capacity() + 5000,
                            self.max_buffer_size as usize,
                        );
                        self.front_buf.grow(new_size);
                    }

                    match self
                        .sock
                        .read(self.front_buf.space())
                        .map_err(ChannelError::Read)?
                    {
                        0 => return Err(ChannelError::NoByteToRead),
                        bytes_read => self.front_buf.fill(bytes_read),
                    };
                }
            }
        }
    }

    fn read_and_parse_from_front_buffer(&mut self, position: usize) -> Result<Rx, ChannelError> {
        let utf8_str = from_utf8(&self.front_buf.data()[..position])
            .map_err(|from_error| ChannelError::InvalidCharSet(from_error.to_string()))?;

        let json_parsed = serde_json::from_str(utf8_str).map_err(ChannelError::Serde)?;

        self.front_buf.consume(position + 1);
        Ok(json_parsed)
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
        let message = match serde_json::to_string(message) {
            Ok(string) => string.into_bytes(),
            Err(_) => Vec::new(),
        };

        let message_len = message.len() + 1;
        if message_len > self.back_buf.available_space() {
            self.back_buf.shift();
        }

        if message_len > self.back_buf.available_space() {
            if message_len - self.back_buf.available_space() + self.back_buf.capacity()
                > (self.max_buffer_size as usize)
            {
                return Err(ChannelError::MessageTooLarge(self.back_buf.capacity()));
            }

            let new_length =
                message_len - self.back_buf.available_space() + self.back_buf.capacity();
            self.back_buf.grow(new_length);
        }

        self.back_buf.write(&message).map_err(ChannelError::Write)?;

        self.back_buf
            .write(&b"\0"[..])
            .map_err(ChannelError::Write)?;

        self.interest.insert(Ready::WRITABLE);

        Ok(())
    }

    /// fills the back buffer with data AND writes on the socket
    fn write_message_blocking(&mut self, message: &Tx) -> Result<(), ChannelError> {
        let message = match serde_json::to_string(message) {
            Ok(string) => string.into_bytes(),
            Err(_) => Vec::new(),
        };

        let msg_len = &message.len() + 1;
        if msg_len > self.back_buf.available_space() {
            self.back_buf.shift();
        }

        if msg_len > self.back_buf.available_space() {
            if msg_len - self.back_buf.available_space() + self.back_buf.capacity()
                > (self.max_buffer_size as usize)
            {
                return Err(ChannelError::MessageTooLarge(self.back_buf.capacity()));
            }

            let new_len = msg_len - self.back_buf.available_space() + self.back_buf.capacity();
            self.back_buf.grow(new_len);
        }

        self.back_buf.write(&message).map_err(ChannelError::Write)?;

        self.back_buf
            .write(&b"\0"[..])
            .map_err(ChannelError::Write)?;

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
}

type ChannelResult<Tx, Rx> = Result<(Channel<Tx, Rx>, Channel<Rx, Tx>), ChannelError>;

impl<Tx: Debug + DeserializeOwned + Serialize, Rx: Debug + DeserializeOwned + Serialize>
    Channel<Tx, Rx>
{
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

impl<Tx: Debug + Serialize, Rx: Debug + DeserializeOwned> Iterator for Channel<Tx, Rx> {
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

    #[derive(Serialize, Deserialize, Debug, PartialEq)]
    struct Serializable(u32);

    #[test]
    fn unblock_a_channel() {
        let (mut blocking, _nonblocking): (
            Channel<Serializable, Serializable>,
            Channel<Serializable, Serializable>,
        ) = Channel::generate(1000, 10000).expect("could not generate blocking channels");
        assert!(blocking.nonblocking().is_ok())
    }

    #[test]
    fn generate_blocking_and_nonblocking_channels() {
        let (blocking_channel, nonblocking_channel): (
            Channel<Serializable, Serializable>,
            Channel<Serializable, Serializable>,
        ) = Channel::generate(1000, 10000).expect("could not generatie blocking channels");

        assert!(blocking_channel.is_blocking());
        assert!(!nonblocking_channel.is_blocking());

        let (nonblocking_channel_1, nonblocking_channel_2): (
            Channel<Serializable, Serializable>,
            Channel<Serializable, Serializable>,
        ) = Channel::generate_nonblocking(1000, 10000)
            .expect("could not generatie nonblocking channels");

        assert!(!nonblocking_channel_1.is_blocking());
        assert!(!nonblocking_channel_2.is_blocking());
    }

    #[test]
    fn write_and_read_message_blocking() {
        let (mut blocking_channel, mut nonblocking_channel): (
            Channel<Serializable, Serializable>,
            Channel<Serializable, Serializable>,
        ) = Channel::generate(1000, 10000).expect("Could not create nonblocking channel");

        let message_to_send = Serializable(42);

        nonblocking_channel
            .blocking()
            .expect("Could not block channel");
        nonblocking_channel
            .write_message(&message_to_send)
            .expect("Could not write message on channel");

        println!("we wrote a message!");

        println!("reading message..");
        // blocking_channel.readable();
        let message = blocking_channel
            .read_message()
            .expect("Could not read message on channel");
        println!("read message!");

        assert_eq!(message, Serializable(42));
    }

    #[test]
    fn read_message_blocking_with_timeout_fails() {
        let (mut reading_channel, mut writing_channel): (
            Channel<Serializable, Serializable>,
            Channel<Serializable, Serializable>,
        ) = Channel::generate(1000, 10000).expect("Could not create nonblocking channel");
        writing_channel.blocking().expect("Could not block channel");

        println!("reading message in a detached thread, with a timeout of 100 milliseconds...");
        let awaiting_with_timeout = thread::spawn(move || {
            let message =
                reading_channel.read_message_blocking_timeout(Some(Duration::from_millis(100)));
            println!("read message!");
            message
        });

        println!("Waiting 200 milliseconds…");
        thread::sleep(std::time::Duration::from_millis(200));

        writing_channel
            .write_message(&Serializable(200))
            .expect("Could not write message on channel");
        println!("we wrote a message that should arrive too late!");

        let arrived_too_late = awaiting_with_timeout
            .join()
            .expect("error with receiving message from awaiting thread");

        assert!(arrived_too_late.is_err());
    }

    #[test]
    fn read_message_blocking_with_timeout_succeeds() {
        let (mut reading_channel, mut writing_channel): (
            Channel<Serializable, Serializable>,
            Channel<Serializable, Serializable>,
        ) = Channel::generate(1000, 10000).expect("Could not create nonblocking channel");
        writing_channel.blocking().expect("Could not block channel");

        println!("reading message in a detached thread, with a timeout of 200 milliseconds...");
        let awaiting_with_timeout = thread::spawn(move || {
            let message = reading_channel
                .read_message_blocking_timeout(Some(Duration::from_millis(200)))
                .expect("Could not read message with timeout on blocking channel");
            println!("read message!");
            message
        });

        println!("Waiting 100 milliseconds…");
        thread::sleep(std::time::Duration::from_millis(100));

        writing_channel
            .write_message(&Serializable(100))
            .expect("Could not write message on channel");
        println!("we wrote a message that should arrive on time!");

        let arrived_on_time = awaiting_with_timeout
            .join()
            .expect("error with receiving message from awaiting thread");

        assert_eq!(arrived_on_time, Serializable(100));
    }

    #[test]
    fn exhaustive_use_of_nonblocking_channels() {
        // - two nonblocking channels A and B, identical
        let (mut channel_a, mut channel_b): (
            Channel<Serializable, Serializable>,
            Channel<Serializable, Serializable>,
        ) = Channel::generate(1000, 10000).expect("Could not create nonblocking channel");
        channel_a.nonblocking().expect("Could not block channel");

        // write on A
        channel_a
            .write_message(&Serializable(1))
            .expect("Could not write message on channel");

        // set B as readable, normally mio tells when to, by giving events
        channel_b.handle_events(Ready::READABLE);

        // read on B
        let should_err = channel_b.read_message();
        assert!(should_err.is_err());

        // write another message on A
        channel_a
            .write_message(&Serializable(2))
            .expect("Could not write message on channel");

        // insert a handle_events Ready::writable on A
        channel_a.handle_events(Ready::WRITABLE);

        // flush A with run()
        channel_a.run().expect("Failed to run the channel");

        // maybe a thread sleep
        // thread::sleep(std::time::Duration::from_millis(100));

        // receive with B using run()
        channel_b.run().expect("Failed to run the channel");

        // use read_message() twice on B, check them
        let message_1 = channel_b
            .read_message()
            .expect("Could not read message on channel");
        assert_eq!(message_1, Serializable(1));

        let message_2 = channel_b
            .read_message()
            .expect("Could not read message on channel");
        assert_eq!(message_2, Serializable(2));
    }
}
