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

use anyhow::{bail, Context};
use mio::{event::Source, net::UnixStream as MioUnixStream};
use serde::{de::DeserializeOwned, ser::Serialize};
use serde_json;

use crate::{buffer::growable::Buffer, ready::Ready};

pub struct Channel<Tx, Rx> {
    pub sock: MioUnixStream,
    front_buf: Buffer,
    pub back_buf: Buffer,
    max_buffer_size: usize,
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
        buffer_size: usize,
        max_buffer_size: usize,
    ) -> anyhow::Result<Channel<Tx, Rx>> {
        let unix_stream = MioUnixStream::connect(path)
            .with_context(|| format!("Could not connect to socket with path {path}"))?;
        Ok(Channel::new(unix_stream, buffer_size, max_buffer_size))
    }

    /// Creates a nonblocking channel, using a unix stream
    pub fn new(sock: MioUnixStream, buffer_size: usize, max_buffer_size: usize) -> Channel<Tx, Rx> {
        Channel {
            sock,
            front_buf: Buffer::with_capacity(buffer_size),
            back_buf: Buffer::with_capacity(buffer_size),
            max_buffer_size,
            readiness: Ready::empty(),
            interest: Ready::readable(),
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
    fn set_nonblocking(&mut self, nonblocking: bool) -> anyhow::Result<()> {
        unsafe {
            let fd = self.sock.as_raw_fd();
            let stream = StdUnixStream::from_raw_fd(fd);
            stream.set_nonblocking(nonblocking).with_context(|| {
                format!("could not change blocking status of unix stream with file descriptor {fd}")
            })?;
            let _fd = stream.into_raw_fd();
        }
        self.blocking = !nonblocking;
        Ok(())
    }

    /// set the channel to be blocking
    pub fn blocking(&mut self) -> anyhow::Result<()> {
        self.set_nonblocking(false)
    }

    /// set the channel to be nonblocking
    pub fn nonblocking(&mut self) -> anyhow::Result<()> {
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
    pub fn run(&mut self) -> anyhow::Result<()> {
        let interest = self.interest & self.readiness;

        if interest.is_readable() {
            let _ = self
                .readable()
                .with_context(|| "error reading from channel")?;
        }

        if interest.is_writable() {
            let _ = self
                .writable()
                .with_context(|| "error writing to channel")?;
        }
        Ok(())
    }

    /// Handles readability by filling the front buffer with the socket data.
    pub fn readable(&mut self) -> anyhow::Result<usize> {
        if !(self.interest & self.readiness).is_readable() {
            bail!("Connection error, continuing")
        }

        let mut count = 0usize;
        loop {
            let size = self.front_buf.available_space();
            if size == 0 {
                self.interest.remove(Ready::readable());
                break;
            }

            match self.sock.read(self.front_buf.space()) {
                Ok(0) => {
                    self.interest = Ready::empty();
                    self.readiness.remove(Ready::readable());
                    self.readiness.insert(Ready::hup());
                    bail!("Error with the socket, reading on it return 0");
                }
                Err(read_error) => match read_error.kind() {
                    ErrorKind::WouldBlock => {
                        self.readiness.remove(Ready::readable());
                        break;
                    }
                    other_error => {
                        self.interest = Ready::empty();
                        self.readiness = Ready::empty();
                        bail!("Error with the socket: {}", other_error);
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
    pub fn writable(&mut self) -> anyhow::Result<usize> {
        if !(self.interest & self.readiness).is_writable() {
            bail!("Connection error, continuing")
        }

        let mut count = 0usize;
        loop {
            let size = self.back_buf.available_data();
            if size == 0 {
                self.interest.remove(Ready::writable());
                break;
            }

            match self.sock.write(self.back_buf.data()) {
                Ok(0) => {
                    self.interest = Ready::empty();
                    self.readiness.insert(Ready::hup());
                    bail!("The write function returned 0");
                }
                Ok(bytes_written) => {
                    count += bytes_written;
                    self.back_buf.consume(bytes_written);
                }
                Err(write_error) => match write_error.kind() {
                    ErrorKind::WouldBlock => {
                        self.readiness.remove(Ready::writable());
                        break;
                    }
                    other_error => {
                        self.interest = Ready::empty();
                        self.readiness = Ready::empty();
                        bail!("channel write error: {:?}", other_error);
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
    pub fn read_message(&mut self) -> anyhow::Result<Rx> {
        if self.blocking {
            self.read_message_blocking()
        } else {
            self.read_message_nonblocking()
        }
    }

    /// Parses a message from the front buffer, without waiting
    fn read_message_nonblocking(&mut self) -> anyhow::Result<Rx> {
        match self.front_buf.data().iter().position(|&x| x == 0) {
            Some(position) => {
                let utf8_str = from_utf8(&self.front_buf.data()[..position])
                    .with_context(|| "invalid utf-8 encoding in command message, ignoring")?;

                let json_parsed = serde_json::from_str(utf8_str)
                    .with_context(|| format!("could not parse message {utf8_str}, ignoring"))?;

                self.front_buf.consume(position + 1);
                Ok(json_parsed)
            }
            None => {
                if self.front_buf.available_space() == 0 {
                    if self.front_buf.capacity() == self.max_buffer_size {
                        error!("command buffer full, cannot grow more, ignoring");
                    } else {
                        let new_size = min(self.front_buf.capacity() + 5000, self.max_buffer_size);
                        self.front_buf.grow(new_size);
                    }
                }

                self.interest.insert(Ready::readable());
                bail!("Could not read anything on the channel");
            }
        }
    }

    fn read_message_blocking(&mut self) -> anyhow::Result<Rx> {
        self.read_message_blocking_timeout(None)
    }

    /// Waits for the front buffer to be filled, and parses a message from it.
    pub fn read_message_blocking_timeout(
        &mut self,
        timeout: Option<Duration>,
    ) -> anyhow::Result<Rx> {
        let now = std::time::Instant::now();

        loop {
            if let Some(timeout) = timeout {
                if now.elapsed() >= timeout {
                    bail!("Timeout is reached: {:?}", timeout);
                }
            }

            match self.front_buf.data().iter().position(|&x| x == 0) {
                Some(position) => {
                    let utf8_str = from_utf8(&self.front_buf.data()[..position])
                        .with_context(|| "invalid utf-8 encoding in command message, ignoring")?;

                    let json_parsed = serde_json::from_str(utf8_str)
                        .with_context(|| format!("could not parse message {utf8_str}, ignoring"))?;

                    self.front_buf.consume(position + 1);
                    return Ok(json_parsed);
                }
                None => {
                    if self.front_buf.available_space() == 0 {
                        if self.front_buf.capacity() == self.max_buffer_size {
                            bail!("command buffer full, cannot grow more, ignoring");
                        }
                        let new_size = min(self.front_buf.capacity() + 5000, self.max_buffer_size);
                        self.front_buf.grow(new_size);
                    }

                    match self
                        .sock
                        .read(self.front_buf.space())
                        .with_context(|| "Error while reading on the socket")?
                    {
                        0 => bail!("No byte letf to read on channel"),
                        bytes_read => self.front_buf.fill(bytes_read),
                    };
                }
            }
        }
    }

    /// Checks whether the channel is blocking or nonblocking, writes the message.
    ///
    /// If the channel is nonblocking, you have to flush using `channel.run()` afterwards
    pub fn write_message(&mut self, message: &Tx) -> anyhow::Result<()> {
        if self.blocking {
            self.write_message_blocking(message)
        } else {
            self.write_message_nonblocking(message)
        }
    }

    /// Writes the message in the buffer, but NOT on the socket.
    /// you have to call channel.run() afterwards
    fn write_message_nonblocking(&mut self, message: &Tx) -> anyhow::Result<()> {
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
                > self.max_buffer_size
            {
                bail!(format!(
                    "message is too large to write to back buffer. Consider increasing proxy channel buffer size, current value is {}",
                    self.back_buf.capacity()
                ));
            }

            let new_length =
                message_len - self.back_buf.available_space() + self.back_buf.capacity();
            self.back_buf.grow(new_length);
        }

        self.back_buf
            .write(&message)
            .with_context(|| "channel could not write to back buffer")?;

        self.back_buf
            .write(&b"\0"[..])
            .with_context(|| "channel could not write to back buffer")?;

        self.interest.insert(Ready::writable());

        Ok(())
    }

    /// fills the back buffer with data AND writes on the socket
    fn write_message_blocking(&mut self, message: &Tx) -> anyhow::Result<()> {
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
                > self.max_buffer_size
            {
                bail!(format!(
                    "message is too large to write to back buffer. Consider increasing proxy channel buffer size, current value is {}",
                    self.back_buf.capacity()
                ));
            }

            let new_len = msg_len - self.back_buf.available_space() + self.back_buf.capacity();
            self.back_buf.grow(new_len);
        }

        self.back_buf
            .write(&message)
            .with_context(|| "channel could not write to back buffer")?;

        self.back_buf
            .write(&b"\0"[..])
            .with_context(|| "channel could not write to back buffer")?;

        loop {
            let size = self.back_buf.available_data();
            if size == 0 {
                break;
            }

            match self.sock.write(self.back_buf.data()) {
                Ok(0) => bail!("No byte written on the channel"),
                Ok(bytes_written) => {
                    self.back_buf.consume(bytes_written);
                }
                Err(_) => return Ok(()), // are we sure?
            }
        }
        Ok(())
    }
}

impl<Tx: Debug + DeserializeOwned + Serialize, Rx: Debug + DeserializeOwned + Serialize>
    Channel<Tx, Rx>
{
    /// creates a channel pair: `(blocking_channel, nonblocking_channel)`
    pub fn generate(
        buffer_size: usize,
        max_buffer_size: usize,
    ) -> anyhow::Result<(Channel<Tx, Rx>, Channel<Rx, Tx>)> {
        let (command, proxy) =
            MioUnixStream::pair().with_context(|| "Could not create a pair of unix streams")?;
        let proxy_channel = Channel::new(proxy, buffer_size, max_buffer_size);
        let mut command_channel = Channel::new(command, buffer_size, max_buffer_size);
        command_channel
            .blocking()
            .with_context(|| "Could not block channel")?;
        Ok((command_channel, proxy_channel))
    }

    /// creates a pair of nonblocking channels
    pub fn generate_nonblocking(
        buffer_size: usize,
        max_buffer_size: usize,
    ) -> io::Result<(Channel<Tx, Rx>, Channel<Rx, Tx>)> {
        let (command, proxy) = MioUnixStream::pair()?;
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
        channel_b.handle_events(Ready::readable());

        // read on B
        let should_err = channel_b.read_message();
        assert!(should_err.is_err());

        // write another message on A
        channel_a
            .write_message(&Serializable(2))
            .expect("Could not write message on channel");

        // insert a handle_events Ready::writable on A
        channel_a.handle_events(Ready::writable());

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
