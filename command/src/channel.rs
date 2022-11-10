use std::{
    cmp::min,
    fmt::Debug,
    io::{self, ErrorKind, Read, Write},
    iter::Iterator,
    marker::PhantomData,
    os::unix::{
        io::{AsRawFd, FromRawFd, IntoRawFd, RawFd},
        net,
    },
    str::from_utf8,
    time::Duration,
};

use mio::{event::Source, net::UnixStream};
use serde::{de::DeserializeOwned, ser::Serialize};
use serde_json;

use crate::{buffer::growable::Buffer, ready::Ready};

#[derive(Debug, PartialEq, Eq)]
pub enum ConnError {
    Continue,
    ParseError,
    SocketError,
}

pub struct Channel<Tx, Rx> {
    pub sock: UnixStream,
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
    ) -> Result<Channel<Tx, Rx>, io::Error> {
        let unix_stream = UnixStream::connect(path)?;
        Ok(Channel::new(unix_stream, buffer_size, max_buffer_size))
    }

    /// Creates a nonblocking channel, using a unix stream
    pub fn new(sock: UnixStream, buffer_size: usize, max_buffer_size: usize) -> Channel<Tx, Rx> {
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

    fn set_nonblocking(&mut self, nonblocking: bool) {
        unsafe {
            let fd = self.sock.as_raw_fd();
            let stream = net::UnixStream::from_raw_fd(fd);
            let _ = stream.set_nonblocking(nonblocking).map_err(|e| {
                error!("could not change blocking status for stream: {:?}", e);
            });
            let _fd = stream.into_raw_fd();
        }
        self.blocking = !nonblocking;
    }

    /// set the channel to be blocking
    pub fn blocking(&mut self) {
        self.set_nonblocking(false)
    }

    /// set the channel to be nonblocking
    pub fn nonblocking(&mut self) {
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

    /// Checks whether we want and can read or write, and calls the appropriate handler.
    pub fn run(&mut self) {
        let interest = self.interest & self.readiness;

        if interest.is_readable() {
            let _ = self.readable().map_err(|e| {
                error!("error reading from channel: {:?}", e);
            });
        }

        if interest.is_writable() {
            let _ = self.writable().map_err(|e| {
                error!("error writing to channel: {:?}", e);
            });
        }
    }

    /// Handles readability by filling the front buffer with the socket data.
    pub fn readable(&mut self) -> Result<usize, ConnError> {
        if !(self.interest & self.readiness).is_readable() {
            return Err(ConnError::Continue);
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
                    //error!("read() returned 0 (count={})", count);
                    self.readiness.insert(Ready::hup());
                    return Err(ConnError::SocketError);
                }
                Err(read_error) => {
                    match read_error.kind() {
                        ErrorKind::WouldBlock => {
                            self.readiness.remove(Ready::readable());
                            break;
                        }
                        _ => {
                            //log!(log::LogLevel::Error, "UNIX CLIENT[{}] read error (kind: {:?}): {:?}", tok.0, code, e);
                            self.interest = Ready::empty();
                            self.readiness = Ready::empty();
                            return Err(ConnError::SocketError);
                        }
                    }
                }
                Ok(bytes_read) => {
                    count += bytes_read;
                    self.front_buf.fill(bytes_read);
                    //debug!("UNIX CLIENT[{}] sent {} bytes: {:?}", tok.0, r, from_utf8(self.buf.data()));
                }
            };
        }

        Ok(count)
    }

    /// Handles writability by writing the content of the back buffer onto the socket
    pub fn writable(&mut self) -> Result<usize, ConnError> {
        if !(self.interest & self.readiness).is_writable() {
            return Err(ConnError::Continue);
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
                    //error!("write() returned 0");
                    self.interest = Ready::empty();
                    self.readiness.insert(Ready::hup());
                    return Err(ConnError::SocketError);
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
                        error!("channel write error: {:?}", other_error);
                        self.interest = Ready::empty();
                        self.readiness = Ready::empty();
                        return Err(ConnError::SocketError);
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
    pub fn read_message(&mut self) -> Option<Rx> {
        if self.blocking {
            self.read_message_blocking()
        } else {
            self.read_message_nonblocking()
        }
    }

    /// Parses a message from the front buffer, without waiting
    fn read_message_nonblocking(&mut self) -> Option<Rx> {
        match self.front_buf.data().iter().position(|&x| x == 0) {
            Some(position) => {
                let mut message_read = None;

                match from_utf8(&self.front_buf.data()[..position]) {
                    Ok(str) => match serde_json::from_str(str) {
                        Ok(message) => message_read = Some(message),
                        Err(serde_error) => error!(
                            "could not parse message (error={:?}), ignoring:\n{}",
                            serde_error, str
                        ),
                    },
                    Err(utf8_error) => error!(
                        "invalid utf-8 encoding in command message, ignoring: {}",
                        utf8_error
                    ),
                }

                self.front_buf.consume(position + 1);
                message_read
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
                None
            }
        }
    }

    fn read_message_blocking(&mut self) -> Option<Rx> {
        self.read_message_blocking_timeout(None)
    }

    /// Waits for the front buffer to be filled, and parses a message from it.
    pub fn read_message_blocking_timeout(&mut self, timeout: Option<Duration>) -> Option<Rx> {
        let now = std::time::Instant::now();

        loop {
            if timeout.is_some() && now.elapsed() >= timeout.unwrap() {
                return None;
            }

            match self.front_buf.data().iter().position(|&x| x == 0) {
                Some(position) => {
                    let mut message_read = None;

                    match from_utf8(&self.front_buf.data()[..position]) {
                        Ok(str) => match serde_json::from_str(str) {
                            Ok(message) => message_read = Some(message),
                            Err(serde_error) => {
                                error!(
                                    "could not parse message (error={:?}), ignoring:\n{}",
                                    serde_error, str
                                );
                            }
                        },
                        Err(utf8_error) => error!(
                            "invalid utf-8 encoding in command message, ignoring:{}",
                            utf8_error
                        ),
                    }

                    self.front_buf.consume(position + 1);
                    return message_read;
                }
                None => {
                    if self.front_buf.available_space() == 0 {
                        if self.front_buf.capacity() == self.max_buffer_size {
                            error!("command buffer full, cannot grow more, ignoring");
                            return None;
                        }
                        let new_size = min(self.front_buf.capacity() + 5000, self.max_buffer_size);
                        self.front_buf.grow(new_size);
                    }

                    match self.sock.read(self.front_buf.space()) {
                        Ok(0) => return None,
                        Err(_) => return None,
                        Ok(bytes_read) => self.front_buf.fill(bytes_read),
                    };
                }
            }
        }
    }

    /// Checks whether the channel is blocking or nonblocking, writes the message.
    ///
    /// If the channel is nonblocking, you have to flush using `channel.run()` afterwards
    pub fn write_message(&mut self, message: &Tx) -> bool {
        if self.blocking {
            self.write_message_blocking(message)
        } else {
            self.write_message_nonblocking(message)
        }
    }

    /// Writes the message in the buffer, but NOT on the socket.
    /// you have to call channel.run() afterwards
    fn write_message_nonblocking(&mut self, message: &Tx) -> bool {
        let message = match serde_json::to_string(message) {
            Ok(string) => string.into_bytes(),
            Err(_) => Vec::new(),
        };

        let msg_len = message.len() + 1;
        if msg_len > self.back_buf.available_space() {
            self.back_buf.shift();
        }

        if msg_len > self.back_buf.available_space() {
            if msg_len - self.back_buf.available_space() + self.back_buf.capacity()
                > self.max_buffer_size
            {
                error!("message is too large to write to back buffer. Consider increasing proxy channel buffer size, current value is {}", self.back_buf.capacity());
                return false;
            }

            let new_len = msg_len - self.back_buf.available_space() + self.back_buf.capacity();
            self.back_buf.grow(new_len);
        }

        if let Err(e) = self.back_buf.write(&message) {
            error!("channel could not write to back buffer: {:?}", e);
            return false;
        }

        if let Err(e) = self.back_buf.write(&b"\0"[..]) {
            error!("channel could not write to back buffer: {:?}", e);
            return false;
        }

        self.interest.insert(Ready::writable());

        true
    }

    /// fills the back buffer with data AND writes on the socket
    fn write_message_blocking(&mut self, message: &Tx) -> bool {
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
                error!(
                    "message is too large to write to back buffer. Consider increasing proxy channel buffer size, current value is {}",
                    self.back_buf.capacity()
                );
                return false;
            }

            let new_len = msg_len - self.back_buf.available_space() + self.back_buf.capacity();
            self.back_buf.grow(new_len);
        }

        if let Err(write_error) = self.back_buf.write(&message) {
            error!("channel could not write to back buffer: {:?}", write_error);
            return false;
        }

        if let Err(write_error) = self.back_buf.write(&b"\0"[..]) {
            error!("channel could not write to back buffer: {:?}", write_error);
            return false;
        }

        loop {
            let size = self.back_buf.available_data();
            if size == 0 {
                break;
            }

            match self.sock.write(self.back_buf.data()) {
                Ok(0) => return false,
                Ok(bytes_written) => {
                    self.back_buf.consume(bytes_written);
                }
                Err(_) => return true,
            }
        }
        true
    }
}

impl<Tx: Debug + DeserializeOwned + Serialize, Rx: Debug + DeserializeOwned + Serialize>
    Channel<Tx, Rx>
{
    /// creates a channel pair: `(blocking_channel, nonblocking_channel)`
    pub fn generate(
        buffer_size: usize,
        max_buffer_size: usize,
    ) -> io::Result<(Channel<Tx, Rx>, Channel<Rx, Tx>)> {
        let (command, proxy) = UnixStream::pair()?;
        let proxy_channel = Channel::new(proxy, buffer_size, max_buffer_size);
        let mut command_channel = Channel::new(command, buffer_size, max_buffer_size);
        command_channel.blocking();
        Ok((command_channel, proxy_channel))
    }

    /// creates a pair of nonblocking channels
    pub fn generate_nonblocking(
        buffer_size: usize,
        max_buffer_size: usize,
    ) -> io::Result<(Channel<Tx, Rx>, Channel<Rx, Tx>)> {
        let (command, proxy) = UnixStream::pair()?;
        let proxy_channel = Channel::new(proxy, buffer_size, max_buffer_size);
        let command_channel = Channel::new(command, buffer_size, max_buffer_size);
        Ok((command_channel, proxy_channel))
    }
}

impl<Tx: Debug + Serialize, Rx: Debug + DeserializeOwned> Iterator for Channel<Tx, Rx> {
    type Item = Rx;
    fn next(&mut self) -> Option<Self::Item> {
        self.read_message()
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
    fn generate_blocking_and_nonblocking_channels() {
        let blocking_channels: io::Result<(
            Channel<Serializable, Serializable>,
            Channel<Serializable, Serializable>,
        )> = Channel::generate(1000, 10000);
        assert!(blocking_channels.is_ok());

        let (blocking_channel, nonblocking_channel) = blocking_channels.unwrap();
        assert!(blocking_channel.is_blocking());
        assert!(!nonblocking_channel.is_blocking());

        let nonblocking_channels: io::Result<(
            Channel<Serializable, Serializable>,
            Channel<Serializable, Serializable>,
        )> = Channel::generate_nonblocking(1000, 10000);

        assert!(nonblocking_channels.is_ok());
        let (channel1, channel2) = nonblocking_channels.unwrap();
        assert!(!channel1.is_blocking());
        assert!(!channel2.is_blocking());
    }

    #[test]
    fn write_and_read_message_blocking() {
        let (mut blocking_channel, mut nonblocking_channel): (
            Channel<Serializable, Serializable>,
            Channel<Serializable, Serializable>,
        ) = Channel::generate(1000, 10000).expect("Could not create nonblocking channel");

        let message_to_send = Serializable(42);

        nonblocking_channel.blocking();
        nonblocking_channel.write_message(&message_to_send);

        println!("we wrote a message!");

        println!("reading message..");
        // blocking_channel.readable();
        let message = blocking_channel.read_message();
        println!("read message!");

        assert_eq!(message, Some(Serializable(42)));
    }

    #[test]
    fn read_message_blocking_with_timeout_fails() {
        let (mut reading_channel, mut writing_channel): (
            Channel<Serializable, Serializable>,
            Channel<Serializable, Serializable>,
        ) = Channel::generate(1000, 10000).expect("Could not create nonblocking channel");
        writing_channel.blocking();

        println!("reading message in a detached thread, with a timeout of 100 milliseconds...");
        let awaiting_with_timeout = thread::spawn(move || {
            let message =
                reading_channel.read_message_blocking_timeout(Some(Duration::from_millis(100)));
            println!("read message!");
            message
        });

        println!("Waiting 200 milliseconds…");
        thread::sleep(std::time::Duration::from_millis(200));

        writing_channel.write_message(&Serializable(200));
        println!("we wrote a message that should arrive too late!");

        let arrived_too_late = awaiting_with_timeout
            .join()
            .expect("error with receiving message from awaiting thread");

        assert_eq!(arrived_too_late, None);
    }

    #[test]
    fn read_message_blocking_with_timeout_succeeds() {
        let (mut reading_channel, mut writing_channel): (
            Channel<Serializable, Serializable>,
            Channel<Serializable, Serializable>,
        ) = Channel::generate(1000, 10000).expect("Could not create nonblocking channel");
        writing_channel.blocking();

        println!("reading message in a detached thread, with a timeout of 200 milliseconds...");
        let awaiting_with_timeout = thread::spawn(move || {
            let message =
                reading_channel.read_message_blocking_timeout(Some(Duration::from_millis(200)));
            println!("read message!");
            message
        });

        println!("Waiting 100 milliseconds…");
        thread::sleep(std::time::Duration::from_millis(100));

        writing_channel.write_message(&Serializable(100));
        println!("we wrote a message that should arrive on time!");

        let arrived_on_time = awaiting_with_timeout
            .join()
            .expect("error with receiving message from awaiting thread");

        assert_eq!(arrived_on_time, Some(Serializable(100)));
    }

    #[test]
    fn exhaustive_use_of_nonblocking_channels() {
        // - two nonblocking channels A and B, identical
        let (mut channel_a, mut channel_b): (
            Channel<Serializable, Serializable>,
            Channel<Serializable, Serializable>,
        ) = Channel::generate(1000, 10000).expect("Could not create nonblocking channel");
        channel_a.nonblocking();

        // write on A
        channel_a.write_message(&Serializable(1));

        // set B as readable, normally mio tells when to, by giving events
        channel_b.handle_events(Ready::readable());

        // read on B
        let should_be_none = channel_b.read_message();
        assert!(should_be_none.is_none());

        // write another message on A
        channel_a.write_message(&Serializable(2));

        // insert a handle_events Ready::writable on A
        channel_a.handle_events(Ready::writable());

        // flush A with run()
        channel_a.run();

        // maybe a thread sleep
        // thread::sleep(std::time::Duration::from_millis(100));

        // receive with B using run()
        channel_b.run();

        // use read_message() twice on B, check them
        let message_1 = channel_b.read_message();
        assert_eq!(message_1, Some(Serializable(1)));

        let message_2 = channel_b.read_message();
        assert_eq!(message_2, Some(Serializable(2)));
    }
}
