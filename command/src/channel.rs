//! Bidirectional length-delimited unix-socket channel.
//!
//! Implements the master ↔ worker / master ↔ CLI message channel: each
//! payload is preceded by a native `usize` length prefix (NOT a NUL
//! separator — that scheme belongs to the state-file save format written
//! by `ConfigState::write_requests_to_file` in `command/src/state.rs`).
//! Bounded by the per-channel `Channel::max_buffer_size`, checked against
//! the declared length in `try_read_delimited_message` before any payload
//! byte is allocated.

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

/// High watermark threshold: log a warning when buffer usage exceeds 80% of max
const HIGH_WATERMARK_RATIO: f64 = 0.8;

#[derive(thiserror::Error, Debug)]
pub enum ChannelError {
    #[error("io read error")]
    Read(std::io::Error),
    #[error("no byte written on the channel")]
    NoByteWritten,
    #[error("no byte left to read on the channel")]
    NoByteToRead,
    #[error(
        "message ({message_len} bytes) too large for back buffer capacity ({capacity} bytes, max {max} bytes)"
    )]
    MessageTooLarge {
        message_len: usize,
        capacity: usize,
        max: usize,
    },
    #[error(
        "declared message length ({message_len} bytes) is shorter than the {delimiter_size}-byte length prefix"
    )]
    MessageLengthUnderDelimiter {
        message_len: usize,
        delimiter_size: usize,
    },
    #[error("channel could not write on the back buffer")]
    Write(std::io::Error),
    #[error("channel buffer is full ({capacity} bytes, max {max} bytes), cannot grow more")]
    BufferFull { capacity: usize, max: usize },
    #[error("Timeout is reached: {0:?}")]
    TimeoutReached(Duration),
    #[error("Could not read anything on the channel")]
    NothingRead,
    #[error("invalid char set in command message, ignoring: {0}")]
    InvalidCharSet(String),
    #[error("could not set the timeout of the unix stream with file descriptor {fd}: {error}")]
    SetTimeout { fd: i32, error: String },
    #[error(
        "Could not change the blocking status ef the unix stream with file descriptor {fd}: {error}"
    )]
    BlockingStatus { fd: i32, error: String },
    #[error("Connection error: {0:?}")]
    Connection(Option<std::io::Error>),
    #[error("Invalid protobuf message: {0}")]
    InvalidProtobufMessage(DecodeError),
    #[error("This should never happen (index out of bound on a tested buffer)")]
    MismatchBufferSize,
}

/// Does this read failure leave a channel that could parse the peer's next
/// frame, if only it were allowed to refill from the socket?
///
/// `readable()` drops `Ready::READABLE` from `interest` whenever it fills
/// `front_buf` to a capacity it may not grow past, and then refuses to run at
/// all while the bit is missing (`if !(self.interest & self.readiness)
/// .is_readable()`). That is correct backpressure, and the parse that frees
/// room is what undoes it -- but until sozu-proxy/sozu#1445 only ONE parse
/// outcome did: `read_message_nonblocking` re-armed on its `NothingRead` path
/// and propagated every error through `?` before reaching it. A frame the
/// parser rejected and then re-synchronised past therefore re-aligned a buffer
/// it could no longer refill, and the bytes still in the socket were stranded
/// on a session `wants_to_tick` (`bin/src/command/sessions.rs`) does not
/// re-schedule for being merely readable.
///
/// The answer is per-variant, not a blanket re-arm before the `?`: a socket
/// that just failed a `read(2)`, or a peer this end has decided to drop, must
/// not have its readability re-asserted. The match below is exhaustive with no
/// wildcard arm ON PURPOSE -- a variant added to `ChannelError` must fail to
/// compile here rather than inherit someone else's disposition.
///
/// Re-arm (the channel retired bytes and is framed on a boundary again; the
/// only thing missing is more bytes):
///
/// - `NothingRead` -- the frame is simply incomplete. The pre-existing case,
///   and the reason the bit is ever re-armed at all.
/// - `MessageLengthUnderDelimiter` -- the parser consumed exactly
///   `delimiter_size()` bytes carrying a declared length no writer can emit for
///   any payload, so those bytes are provably not a header and skipping them
///   re-aligns the stream. Re-syncing a buffer that may never refill only
///   re-syncs the bytes already in hand, which is the case #1445 was filed on.
/// - `InvalidProtobufMessage` -- since sozu-proxy/sozu#1428 the whole frame is
///   consumed before the decode failure is returned, so the buffer is left
///   exactly as the decode-success path leaves it. The frame is lost; the
///   channel is not.
///
/// Do not re-arm:
///
/// - `MessageTooLarge` -- twice not, and the first reason does not depend on
///   the second. It consumes NOTHING: unlike its two siblings above it cannot,
///   because the bytes behind the header may be payload rather than a fresh
///   delimiter and re-framing on them would decode peer-chosen bytes as a
///   control-plane request. So the buffer is not on a boundary and refilling
///   only re-parses the same header -- a busy loop, not a recovery, and the
///   frame could not complete anyway, a length above `max_buffer_size` not
///   fitting a buffer bounded by `max_buffer_size` however much more is read.
///   On top of that the branch has marked `Ready::ERROR` since #1428, so
///   re-arming would assert readability on a channel this end has already
///   decided to close.
/// - `BufferFull` -- past #1436's compaction this arm means a compacted buffer
///   at the ceiling whose whole capacity cannot hold a length prefix, so
///   `available_space()` is zero and cannot grow. `readable()` would read zero
///   bytes and immediately remove the bit again; refilling cannot change the
///   outcome.
/// - `MismatchBufferSize` -- "this should never happen": a `try_into` on a
///   slice whose length was just checked against `delimiter_size()`. It is an
///   internal invariant violation, and the same slice fails identically on
///   every retry, so re-arming would spin instead of recovering.
/// - `NoByteToRead` -- `read(2)` returned zero, the peer is gone.
///   `readable()` has already set `interest = Ready::EMPTY` and raised HUP;
///   re-arming would contradict the hangup it just recorded.
/// - `Read` -- a hard socket failure (`writable()` reports its write errors
///   under this variant too), after which the failing call has already
///   cleared `interest` and raised HUP and ERROR (sozu-proxy/sozu#1560).
/// - `Connection` -- either a failed `connect(2)` or `readable()`/`writable()`
///   rejecting the call because the interest gate is shut. Restoring from here
///   the very bit that gate just refused is the loop this function exists to
///   avoid.
/// - `NoByteWritten`, `Write` -- back-buffer/socket write failures, raised on
///   the write path and terminal there (`NoByteWritten` raises HUP); nothing
///   about them says the read side may refill.
/// - `TimeoutReached` -- the blocking read path's deadline
///   (`read_message_blocking_timeout`), an operator-visible bound rather than a
///   framing state.
/// - `SetTimeout`, `BlockingStatus` -- `setsockopt`/`fcntl` failures on the
///   underlying stream, i.e. blocking-mode plumbing.
/// - `InvalidCharSet` -- declared here but constructed nowhere; the live
///   variant of that name is `ScmSocketError::InvalidCharSet`
///   (`command/src/scm_socket.rs`). It is listed so the match stays exhaustive,
///   and it is not a framing state either way.
///
/// Exactly six of the fifteen reach this function today.
/// `try_read_delimited_message` raises five of them -- `MessageTooLarge`,
/// `MessageLengthUnderDelimiter`, `InvalidProtobufMessage`, `BufferFull` and
/// `MismatchBufferSize` -- and `read_message_nonblocking` mints the sixth,
/// `NothingRead`, from its `Ok(None)`. The other nine (`NoByteToRead`, `Read`,
/// `Connection`, `NoByteWritten`, `Write`, `TimeoutReached`, `SetTimeout`,
/// `BlockingStatus`, `InvalidCharSet`) are dispositioned anyway rather than
/// left to a wildcard, because a future call site is exactly how a wildcard
/// becomes a wrong answer nobody wrote down.
fn rearms_readable(error: &ChannelError) -> bool {
    match error {
        ChannelError::NothingRead
        | ChannelError::MessageLengthUnderDelimiter { .. }
        | ChannelError::InvalidProtobufMessage(_) => true,

        ChannelError::MessageTooLarge { .. }
        | ChannelError::BufferFull { .. }
        | ChannelError::MismatchBufferSize
        | ChannelError::NoByteToRead
        | ChannelError::Read(_)
        | ChannelError::Connection(_)
        | ChannelError::NoByteWritten
        | ChannelError::Write(_)
        | ChannelError::TimeoutReached(_)
        | ChannelError::SetTimeout { .. }
        | ChannelError::BlockingStatus { .. }
        | ChannelError::InvalidCharSet(_) => false,
    }
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
    initial_buffer_size: usize,
    max_buffer_size: usize,
    pub readiness: Ready,
    pub interest: Ready,
    blocking: bool,
    /// true if a high watermark warning has been logged for the front buffer
    front_high_watermark_logged: bool,
    /// true if a high watermark warning has been logged for the back buffer
    back_high_watermark_logged: bool,
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
    ///
    /// `buffer_size` is clamped to `max_buffer_size` (sozu-proxy/sozu#1416):
    /// every growth/shrink path in this file — `grow_size`, `try_read_delimited_message`,
    /// `try_shrink_front_buf`/`try_shrink_back_buf` — reasons about `front_buf`/`back_buf`
    /// capacity never exceeding `max_buffer_size`; a caller-supplied `buffer_size` above
    /// that ceiling would start the channel already violating the invariant those paths
    /// assert, before a single byte is read.
    ///
    /// A clamp, not a fallible constructor, because `max_buffer_size` and
    /// `initial_buffer_size` are private fields with no external struct-literal path
    /// around this function: `Channel::new` is the *only* place a `Channel` is built, so
    /// unlike `H2FloodConfig::new` (sozu-proxy/sozu#1418, whose clamp is bypassed in
    /// production by a raw struct literal in `get_h2_flood_config`) this clamp cannot be
    /// routed around — it runs on every construction, including the dozen-plus call sites
    /// across `bin/`, `e2e/`, `lib/examples/` and this module's own tests. Config load
    /// (`ConfigBuilder::into_config`) is the operator-facing gate and rejects the
    /// misconfiguration with a named error before a `Channel` is ever built; this clamp is
    /// the structural backstop for any other direct caller that bypasses config
    /// validation (tests, examples, a future call site), so it logs instead of silently
    /// doing nothing observable.
    pub fn new(sock: MioUnixStream, buffer_size: u64, max_buffer_size: u64) -> Channel<Tx, Rx> {
        let buffer_size = buffer_size as usize;
        let max_buffer_size = max_buffer_size as usize;
        let initial_buffer_size = min(buffer_size, max_buffer_size);
        if initial_buffer_size < buffer_size {
            warn!(
                "channel buffer_size ({}) exceeds max_buffer_size ({}); clamping initial \
                 buffer capacity to {}",
                buffer_size, max_buffer_size, initial_buffer_size
            );
        }
        // Postcondition: the invariant every growth/shrink path in this file assumes.
        debug_assert!(
            initial_buffer_size <= max_buffer_size,
            "initial buffer capacity must never exceed max_buffer_size"
        );
        Channel {
            sock,
            front_buf: Buffer::with_capacity(initial_buffer_size),
            back_buf: Buffer::with_capacity(initial_buffer_size),
            initial_buffer_size,
            max_buffer_size,
            readiness: Ready::EMPTY,
            interest: Ready::READABLE,
            blocking: false,
            front_high_watermark_logged: false,
            back_high_watermark_logged: false,
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
            initial_buffer_size: self.initial_buffer_size,
            max_buffer_size: self.max_buffer_size,
            readiness: self.readiness,
            interest: self.interest,
            blocking: self.blocking,
            front_high_watermark_logged: self.front_high_watermark_logged,
            back_high_watermark_logged: self.back_high_watermark_logged,
            phantom_tx: PhantomData,
            phantom_rx: PhantomData,
        }
    }

    // Since MioUnixStream does not have a set_nonblocking method, we have to use the standard library.
    // We get the file descriptor of the MioUnixStream socket, create a standard library UnixStream,
    // set it to nonblocking, let go of the file descriptor
    fn set_nonblocking(&mut self, nonblocking: bool) -> Result<(), ChannelError> {
        // SAFETY: `fd` is borrowed from `self.sock` for the duration of this
        // block. We wrap it in a `StdUnixStream` to call `set_nonblocking`,
        // then immediately release ownership again with `into_raw_fd` so the
        // descriptor is not closed by `Drop`. `self.sock` retains the
        // original ownership.
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
        // SAFETY: `fd` is borrowed from `self.sock` for the duration of this
        // block. We wrap it in a `StdUnixStream` to call `set_read_timeout`,
        // then immediately release ownership again with `into_raw_fd` so the
        // descriptor is not closed by `Drop`. `self.sock` retains the
        // original ownership.
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

    /// Compute the next buffer size using a doubling strategy, capped at max_buffer_size.
    /// Returns None if the buffer is already at max capacity.
    fn grow_size(&self, current_capacity: usize) -> Option<usize> {
        if current_capacity >= self.max_buffer_size {
            return None;
        }
        // Precondition: we only reach here below the ceiling, so a strictly
        // larger size always exists within [current+1, max_buffer_size].
        debug_assert!(
            current_capacity < self.max_buffer_size,
            "grow_size must only grow buffers that are strictly below the max ceiling"
        );
        // double the capacity, but don't exceed max
        let new_size = min(current_capacity.saturating_mul(2), self.max_buffer_size);
        // ensure we grow by at least something (in case current_capacity is 0)
        let new_size = new_size.max(current_capacity + 1);
        let new_size = min(new_size, self.max_buffer_size);
        // Postconditions: the new capacity strictly grows (forward progress,
        // no spin) and never overshoots the configured ceiling.
        debug_assert!(
            new_size > current_capacity,
            "grow_size must make forward progress (new capacity strictly larger)"
        );
        debug_assert!(
            new_size <= self.max_buffer_size,
            "grow_size must never exceed the configured max_buffer_size ceiling"
        );
        Some(new_size)
    }

    /// Check if a buffer has exceeded the high watermark and log a warning once
    fn check_high_watermark(
        buffer_name: &str,
        capacity: usize,
        max: usize,
        already_logged: &mut bool,
    ) {
        if *already_logged {
            return;
        }
        let threshold = (max as f64 * HIGH_WATERMARK_RATIO) as usize;
        if capacity >= threshold {
            warn!(
                "channel {} buffer reached high watermark: {} bytes ({:.0}% of {} max)",
                buffer_name,
                capacity,
                (capacity as f64 / max as f64) * 100.0,
                max,
            );
            *already_logged = true;
        }
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
    /// Grows the front buffer when full using a doubling strategy, up to max_buffer_size.
    pub fn readable(&mut self) -> Result<usize, ChannelError> {
        if !(self.interest & self.readiness).is_readable() {
            return Err(ChannelError::Connection(None));
        }

        let mut count = 0usize;
        loop {
            let size = self.front_buf.available_space();
            trace!("channel available space: {}", size);
            if size == 0 {
                // try to grow the buffer before giving up
                if let Some(new_size) = self.grow_size(self.front_buf.capacity()) {
                    Self::check_high_watermark(
                        "front",
                        new_size,
                        self.max_buffer_size,
                        &mut self.front_high_watermark_logged,
                    );
                    self.front_buf.grow(new_size);
                    // The read buffer must never grow past the configured ceiling.
                    debug_assert!(
                        self.front_buf.capacity() <= self.max_buffer_size,
                        "front buffer capacity must stay within the max_buffer_size ceiling"
                    );
                } else {
                    self.interest.remove(Ready::READABLE);
                    break;
                }
            }

            // The slice handed to `read` is exactly the free tail; the kernel
            // can never write more than that, so a successful read of N bytes
            // is bounded by the space we just measured (post-grow).
            let space_before = self.front_buf.available_space();
            debug_assert!(
                space_before > 0,
                "readable must only call read() with non-empty space (zero-space path grows or breaks)"
            );
            let data_before = self.front_buf.available_data();
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
                        // Mark the channel for closing, as the `Ok(0)` arm
                        // does: the owner closes on HUP or ERROR, and an
                        // edge-triggered poller will not report them again
                        // (sozu-proxy/sozu#1560). Assignment, not `insert`:
                        // the socket has failed, so no other bit may stay
                        // armed.
                        self.interest = Ready::EMPTY;
                        self.readiness = Ready::HUP | Ready::ERROR;
                        return Err(ChannelError::Read(read_error));
                    }
                },
                Ok(bytes_read) => {
                    // A read can never deliver more bytes than the free space
                    // it was handed; otherwise `fill` would corrupt offsets.
                    debug_assert!(
                        bytes_read <= space_before,
                        "read delivered more bytes than the buffer space it was given"
                    );
                    count += bytes_read;
                    self.front_buf.fill(bytes_read);
                    // `fill` advances `end` by exactly the bytes read, so the
                    // available data grows by that delta — pair-assert the
                    // offset mutation.
                    debug_assert_eq!(
                        self.front_buf.available_data(),
                        data_before + bytes_read,
                        "front buffer available_data must increase by exactly bytes_read"
                    );
                }
            };
        }

        Ok(count)
    }

    /// Handle writability by writing the content of the back buffer onto the socket.
    /// Shrinks the back buffer back toward initial size once fully drained.
    pub fn writable(&mut self) -> Result<usize, ChannelError> {
        if !(self.interest & self.readiness).is_writable() {
            return Err(ChannelError::Connection(None));
        }

        let mut count = 0usize;
        loop {
            let size = self.back_buf.available_data();
            if size == 0 {
                self.interest.remove(Ready::WRITABLE);
                self.try_shrink_back_buf();
                break;
            }

            let data_before = self.back_buf.available_data();
            match self.sock.write(self.back_buf.data()) {
                Ok(0) => {
                    self.interest = Ready::EMPTY;
                    self.readiness.insert(Ready::HUP);
                    return Err(ChannelError::NoByteWritten);
                }
                Ok(bytes_written) => {
                    // The kernel cannot accept more than the slice (`data()`)
                    // it was handed; otherwise `consume` would underflow.
                    debug_assert!(
                        bytes_written <= data_before,
                        "write reported more bytes than the buffer data it was given"
                    );
                    count += bytes_written;
                    let consumed = self.back_buf.consume(bytes_written);
                    // `consume` is saturating but the bound above guarantees an
                    // exact consume here; pair-assert the offset mutation.
                    debug_assert_eq!(
                        consumed, bytes_written,
                        "back buffer must consume exactly the bytes written to the socket"
                    );
                    debug_assert_eq!(
                        self.back_buf.available_data(),
                        data_before - bytes_written,
                        "back buffer available_data must shrink by exactly bytes_written"
                    );
                }
                Err(write_error) => match write_error.kind() {
                    ErrorKind::WouldBlock => {
                        self.readiness.remove(Ready::WRITABLE);
                        break;
                    }
                    _ => {
                        // Same as the read side: keep the channel marked for
                        // closing instead of wiping HUP and ERROR. WRITABLE
                        // must go: the worker's `Server::send_queue` keeps
                        // looping while it is set and data is pending.
                        self.interest = Ready::EMPTY;
                        self.readiness = Ready::HUP | Ready::ERROR;
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
        // `NothingRead` is not a special case, it is one row of
        // [`rearms_readable`]'s table: an incomplete frame and a frame the
        // parser rejected and re-synchronised past are the same situation from
        // `readable()`'s point of view, and propagating the second through `?`
        // before the re-arm is what stranded the socket bytes behind it
        // (sozu-proxy/sozu#1445).
        let error = match self.try_read_delimited_message() {
            Ok(Some(message)) => {
                self.try_shrink_front_buf();
                return Ok(message);
            }
            Ok(None) => ChannelError::NothingRead,
            Err(error) => error,
        };

        if rearms_readable(&error) {
            self.interest.insert(Ready::READABLE);
        }
        Err(error)
    }

    /// Wait for the front buffer to be filled, and parses a message from it.
    pub fn read_message_blocking_timeout(
        &mut self,
        timeout: Option<Duration>,
    ) -> Result<Rx, ChannelError> {
        let now = std::time::Instant::now();

        // 10 ms = 100 syscalls/sec on idle WouldBlock, pinning a CPU on
        // long blocking waits with no payload. 100 ms is
        // a usability-acceptable resolution for the outer `timeout`
        // deadline check (the wait is bounded by `timeout`, not by this
        // value) and drops the steady-state read syscall rate to 10/sec.
        self.set_timeout(Some(Duration::from_millis(100)))?;

        let status = loop {
            if let Some(timeout) = timeout
                && now.elapsed() >= timeout
            {
                break Err(ChannelError::TimeoutReached(timeout));
            }

            if let Some(message) = self.try_read_delimited_message()? {
                self.try_shrink_front_buf();
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
        // Invariant guarding all the slice indexing below: the front buffer can
        // never have grown past the configured ceiling. Every grow site routes
        // through `grow_size`/`max_buffer_size`; if this ever fired the length
        // checks would be reasoning against a stale bound.
        debug_assert!(
            self.front_buf.capacity() <= self.max_buffer_size,
            "front buffer capacity must never exceed max_buffer_size"
        );
        let buffer = self.front_buf.data();
        // `data()` returns `memory[position..end]`, so its length is exactly the
        // available data and can never exceed the buffer capacity.
        debug_assert!(
            buffer.len() <= self.front_buf.capacity(),
            "available data slice cannot exceed buffer capacity"
        );
        if buffer.len() >= delimiter_size() {
            let delimiter = buffer[..delimiter_size()]
                .try_into()
                .map_err(|_| ChannelError::MismatchBufferSize)?;
            let message_len = usize::from_le_bytes(delimiter);

            // Defense in depth: bound the parser-side length up-front.
            // Without this an attacker who controls the
            // first 8 bytes of a frame can declare an arbitrarily large
            // message and drive `Buffer::grow` toward the
            // `max_buffer_size` ceiling before any byte of payload has
            // been read. Reject as `MessageTooLarge` before the doubling
            // growth strategy ever runs on attacker-supplied numbers.
            //
            // Unlike the `MessageLengthUnderDelimiter` case below, this one
            // must NOT consume and re-sync. That one can: a declared length
            // under `delimiter_size()` is a value no writer can produce for
            // any payload, so those bytes are provably not a real header and
            // skipping exactly them re-aligns the stream. Here the declared
            // length may be honest, in which case the bytes behind the header
            // are payload -- dropping the header and re-framing on them would
            // decode peer-chosen bytes as a fresh control-plane request. And
            // the frame can never complete either way: a length above
            // `max_buffer_size` does not fit a buffer bounded by
            // `max_buffer_size`, however much more is read.
            //
            // So it is fatal for that peer, which is already the stance taken
            // for the mirror condition on the write side
            // (`is_transient_overflow`, `bin/src/command/sessions.rs`: "a
            // frame bigger than the ceiling itself, which no amount of
            // draining will ever admit"). That is a statement about this end,
            // NOT about the peer's good faith: the two ends size their
            // channels from their own configuration, so a peer built with a
            // larger `max_command_buffer_size` emits frames this end refuses
            // while conforming perfectly -- `write_delimited_message` below
            // bounds an outgoing frame against the *writer's* ceiling, and the
            // two configurations shipped here already disagree tenfold
            // (`bin/config.toml` 163_840, `os-build/config.toml` 1_638_400).
            // A `sozu ctl --config` pointed at one while the supervisor was
            // started from the other, or two binaries built with different
            // `SOZU_CONFIG` defaults (`bin/build.rs`, `bin/src/util.rs`), is
            // all it takes. Closing is still the only correct exit for this
            // end, which can neither complete nor
            // re-sync the frame; the log line says what to reconcile instead
            // of implying an attack. Mark the channel errored -- the single
            // signal `ClientSession::ready`, `WorkerSession::ready` and
            // `wants_to_tick` all key on -- so the supervisor drops the
            // session. Without it the header is re-parsed forever,
            // `extract_messages` swallows the error with a bare `Err(_)`, and
            // the session wedges while pinning its file descriptor and buffer
            // (sozu-proxy/sozu#1428).
            //
            // `insert`, never assignment: on the worker's own channel
            // (`lib/src/server.rs`) `interest` carries only READABLE and
            // WRITABLE, so `readiness()` masks ERROR away and nothing there
            // reads `is_error()`. An assignment's only effect on that side
            // would be clearing WRITABLE, and `Server::send_queue` gates on
            // `channel.readiness.is_writable()`, so already-queued worker
            // responses would stall until the next edge-triggered writability
            // event.
            if message_len > self.max_buffer_size {
                error!(
                    "peer declared a {}-byte frame, above this end's {}-byte ceiling; closing \
                     the connection. The two ends size their channels independently, so this is \
                     more often a configuration mismatch than a hostile peer: reconcile \
                     `max_command_buffer_size` (and `command_buffer_size`, which must stay below \
                     it) between this end and the peer's",
                    message_len, self.max_buffer_size
                );
                self.readiness.insert(Ready::ERROR);
                return Err(ChannelError::MessageTooLarge {
                    message_len,
                    capacity: self.front_buf.capacity(),
                    max: self.max_buffer_size,
                });
            }

            // A length-delimited frame is `[delimiter][payload]`. The declared
            // `message_len` is the total frame size and MUST therefore be at
            // least `delimiter_size()`. A peer-controlled value below that
            // ceiling makes `&buffer[delimiter_size()..message_len]` slice
            // backwards and panic; reject it the same way as oversized frames.
            //
            // Drop the bogus delimiter bytes before returning. Without this,
            // every subsequent `read_message()` re-reads the same bad header
            // from the front buffer and the worker burns CPU on the same error
            // until the peer disconnects.
            //
            // This re-aligns what is already in `front_buf`, and nothing more;
            // `rearms_readable` above carries the refill half.
            if message_len < delimiter_size() {
                self.front_buf.consume(delimiter_size());
                return Err(ChannelError::MessageLengthUnderDelimiter {
                    message_len,
                    delimiter_size: delimiter_size(),
                });
            }

            if buffer.len() >= message_len {
                // By the time we slice, the two guards above have proven the
                // length prefix is well-formed: it is at least the delimiter
                // (so `delimiter_size()..message_len` runs forward) and at most
                // the configured ceiling (so it cannot drive growth). The
                // `buffer.len() >= message_len` branch then guarantees the whole
                // frame is in the buffer. These are the exact invariants that
                // keep the slice and the `consume` in bounds — never reachable
                // from a malformed length, which already returned an error.
                debug_assert!(
                    message_len >= delimiter_size(),
                    "decode path requires a frame at least as large as its delimiter"
                );
                debug_assert!(
                    message_len <= self.max_buffer_size,
                    "decode path requires the declared length within the max ceiling"
                );
                debug_assert!(
                    message_len <= buffer.len(),
                    "decode path requires the full frame to be buffered before slicing"
                );
                let available_before = self.front_buf.available_data();
                debug_assert_eq!(
                    available_before,
                    buffer.len(),
                    "available_data must equal the data slice length we validated against"
                );
                // Decode first, consume unconditionally, propagate a decode
                // failure only afterwards. Returning before the consume (as
                // this did until sozu-proxy/sozu#1428) left the whole frame at
                // the head of the buffer and moved no readiness bit, and
                // `extract_messages` (`bin/src/command/sessions.rs`) swallows
                // the error with its bare `Err(_)` arm, so the session
                // re-decoded the same bytes on every later read and wedged.
                //
                // Note what does NOT justify this. `message_len` is only
                // range-checked above -- at least `delimiter_size()`, at most
                // `max_buffer_size`, no larger than what is buffered. It is
                // not proven to be a real frame boundary, so on a
                // desynchronised stream consuming it re-frames on bytes the
                // sender never framed, which is the very hazard that argues
                // against consuming in the `MessageTooLarge` branch above.
                // Nor is this the `MessageLengthUnderDelimiter` stance: that
                // one consumes exactly `delimiter_size()` on a value no writer
                // can emit for any payload, an impossibility proof this branch
                // does not have.
                //
                // What justifies it is coherence with the success path four
                // lines down, which already consumes this same peer-supplied
                // `message_len` and re-frames on whatever follows. The
                // desynchronisation hazard is therefore identical whether the
                // payload decodes or not, and it predates this change.
                // Closing the peer only on the failing half would punish the
                // case more likely to be honest protobuf skew while silently
                // accepting the case that is not.
                let decoded = Rx::decode(&buffer[delimiter_size()..message_len]);
                let consumed = self.front_buf.consume(message_len);
                // The whole frame (delimiter + payload) is consumed exactly:
                // pair-assert that consume advanced by message_len and the data
                // pointer moved forward by the same amount.
                debug_assert_eq!(
                    consumed, message_len,
                    "must consume exactly the validated frame length"
                );
                debug_assert_eq!(
                    self.front_buf.available_data(),
                    available_before - message_len,
                    "available_data must drop by exactly the consumed frame length"
                );
                let message = decoded.map_err(|decode_error| {
                    error!(
                        "could not decode a {}-byte frame from the peer; that frame is dropped \
                         and the channel re-syncs on the next one: {}",
                        message_len, decode_error
                    );
                    ChannelError::InvalidProtobufMessage(decode_error)
                })?;
                return Ok(Some(message));
            }
        }

        if self.front_buf.available_space() == 0 {
            // Compact before concluding anything about the space (fixes
            // sozu-proxy/sozu#1436). `available_space()` is `capacity - end`,
            // so it measures the free TAIL, not the free room: a buffer whose
            // `position` sits mid-way reports zero space while holding
            // `position` reusable bytes at its head. `Buffer::consume`
            // (`command/src/buffer/growable.rs`) only compacts past
            // `capacity / 2`, so decoding one frame of at most half the
            // capacity out of a buffer filled to capacity leaves exactly that
            // layout, and `Buffer::shift` hands the room straight back.
            //
            // Without it this arm returned `BufferFull` without consuming and
            // without moving a readiness bit -- the shape
            // sozu-proxy/sozu#1428 removed from the two branches above -- and
            // no later call could recover: `readable()` sees
            // `available_space() == 0`, gets `None` from `grow_size` at the
            // ceiling and executes `interest.remove(Ready::READABLE)`, so
            // every later `readable()` returns `Err(Connection(None))` and
            // `fill` is out of reach; `consume` needs a decoded frame or an
            // under-delimiter length, which this return path never reaches;
            // and `try_shrink_front_buf` is never reached, because both
            // `read_message` paths call it only after a frame decoded, and
            // both of its own early returns would stop it anyway --
            // `capacity <= initial_buffer_size` whenever the channel was
            // configured with `command_buffer_size == max_command_buffer_size`,
            // and `available_data() * 4 < initial_buffer_size`, which is false
            // with that much pending. Not because it shrinks rather than
            // shifts: `Buffer::shrink` calls `shift` unconditionally before its
            // own size bail, so reaching it WOULD have compacted. The session
            // then wedged permanently, holding its file descriptor and a full
            // buffer, on a peer that had done nothing wrong.
            //
            // Compacting here also changes the sub-ceiling path: a front buffer
            // still below `max_buffer_size` now compacts on its first zero-space
            // parse and grows on the next one, rather than doubling straight
            // away. The frame still completes -- the compacted buffer refills to
            // `end == capacity` at `position == 0`, where the shift is a no-op
            // and the grow below runs -- one read-parse round later, against one
            // fewer doubling.
            //
            // Compaction is the whole recovery, NOT a close: the frame is
            // under this end's ceiling and its buffered bytes are intact, so
            // only the buffer's internal offsets stand in the way. It cannot
            // mask an unsatisfiable frame either -- a declared length above
            // `max_buffer_size` returns `MessageTooLarge` at the guard above,
            // before control ever arrives here, so the ordering of the two is
            // what keeps "too large for the ceiling" and "too large for the
            // current layout" apart. The write side already compacts on the
            // same reasoning before it considers growing
            // (`write_delimited_message` below).
            //
            // Compacting makes the channel recoverable; it does not by itself
            // make a session recover. Freeing space without changing capacity
            // ended `extract_messages`' drain loop (`bin/src/command/sessions.rs`)
            // on the very parse that made the room, one read short of completing
            // the frame, and nothing re-schedules a merely-readable session. That
            // loop's termination anchor was corrected in the same changeset; a
            // change to what this arm does to the buffer without changing
            // capacity has to be checked against it.
            self.front_buf.shift();
        }

        if self.front_buf.available_space() == 0 {
            if self.front_buf.capacity() >= self.max_buffer_size {
                // Past the compaction above, zero space means `position == 0`
                // and `end == capacity`, so the whole capacity is pending data;
                // with `capacity == max_buffer_size` any in-range declared
                // length would have decoded already. Only a capacity too small
                // to hold a length prefix at all reaches this -- a
                // configuration that can never frame a message, not a buffer
                // that has run out of room.
                debug_assert!(
                    self.front_buf.available_data() < delimiter_size(),
                    "a compacted, full buffer at the ceiling can only fail to parse when its capacity cannot hold a length prefix"
                );
                return Err(ChannelError::BufferFull {
                    capacity: self.front_buf.capacity(),
                    max: self.max_buffer_size,
                });
            }
            let new_size = self
                .grow_size(self.front_buf.capacity())
                .unwrap_or(self.max_buffer_size);
            Self::check_high_watermark(
                "front",
                new_size,
                self.max_buffer_size,
                &mut self.front_high_watermark_logged,
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

        // The framed length is the payload plus a fixed delimiter, so it is by
        // construction at least one delimiter wide — the mirror of the
        // `MessageLengthUnderDelimiter` invariant the reader enforces.
        debug_assert!(
            payload_len >= delimiter_size(),
            "framed length must include the fixed-size delimiter prefix"
        );

        let delimiter = payload_len.to_le_bytes();

        if payload_len > self.back_buf.available_space() {
            self.back_buf.shift();
        }

        let data_before = self.back_buf.available_data();
        if payload_len > self.back_buf.available_space() {
            let needed = payload_len - self.back_buf.available_space() + self.back_buf.capacity();
            if needed > self.max_buffer_size {
                return Err(ChannelError::MessageTooLarge {
                    message_len: payload_len,
                    capacity: self.back_buf.capacity(),
                    max: self.max_buffer_size,
                });
            }
            // Past the ceiling check, the required size is within the ceiling
            // and at least the current capacity (we only enter on shortfall).
            debug_assert!(
                needed <= self.max_buffer_size,
                "grow target must be within the max ceiling once the cap check passed"
            );

            let capacity_before = self.back_buf.capacity();
            // use doubling strategy to reach at least `needed`, amortizing future writes
            let mut new_length = self.back_buf.capacity();
            while new_length < needed {
                new_length = new_length.saturating_mul(2).max(new_length + 1);
            }
            new_length = min(new_length, self.max_buffer_size);
            // Post-grow target: large enough to fit the frame yet capped at the
            // configured ceiling and never below where we started.
            debug_assert!(
                new_length >= needed,
                "doubling growth must reach at least the needed capacity"
            );
            debug_assert!(
                new_length <= self.max_buffer_size,
                "grown back buffer must stay within the max_buffer_size ceiling"
            );
            debug_assert!(
                new_length >= capacity_before,
                "growth must never shrink the back buffer"
            );
            Self::check_high_watermark(
                "back",
                new_length,
                self.max_buffer_size,
                &mut self.back_high_watermark_logged,
            );
            self.back_buf.grow(new_length);
            // After the grow the frame must fit in the now-available space.
            debug_assert!(
                payload_len <= self.back_buf.available_space(),
                "back buffer must have room for the full frame after growth"
            );
        }

        self.back_buf
            .write_all(&delimiter)
            .map_err(ChannelError::Write)?;
        self.back_buf
            .write_all(&payload)
            .map_err(ChannelError::Write)?;

        // The two writes appended exactly `payload_len` bytes (delimiter +
        // payload) to the back buffer's pending data.
        debug_assert_eq!(
            self.back_buf.available_data(),
            data_before + payload_len,
            "back buffer pending data must grow by exactly the framed length"
        );
        debug_assert!(
            self.back_buf.capacity() <= self.max_buffer_size,
            "back buffer capacity must never exceed the max_buffer_size ceiling"
        );

        Ok(())
    }

    /// Shrink the front buffer back toward initial_buffer_size when it is
    /// mostly empty (data consumed) and was previously grown.
    fn try_shrink_front_buf(&mut self) {
        let capacity = self.front_buf.capacity();
        if capacity <= self.initial_buffer_size {
            return;
        }
        // Past the early return, we are strictly above the floor, so a shrink
        // back to `initial_buffer_size` is a genuine reduction.
        debug_assert!(
            capacity > self.initial_buffer_size,
            "shrink path only runs when capacity is above the initial floor"
        );
        // only shrink when the buffer has little pending data
        if self.front_buf.available_data() * 4 < self.initial_buffer_size {
            let data_before = self.front_buf.available_data();
            self.front_buf.shrink(self.initial_buffer_size);
            self.front_high_watermark_logged = false;
            // Shrink preserves pending data and never drops below the floor.
            debug_assert!(
                self.front_buf.capacity() >= self.initial_buffer_size,
                "front buffer must never shrink below the initial buffer size floor"
            );
            debug_assert_eq!(
                self.front_buf.available_data(),
                data_before,
                "shrink must preserve all pending front-buffer data"
            );
            trace!(
                "front buffer shrunk from {} to {} bytes",
                capacity, self.initial_buffer_size
            );
        }
    }

    /// Shrink the back buffer back toward initial_buffer_size when fully drained.
    fn try_shrink_back_buf(&mut self) {
        let capacity = self.back_buf.capacity();
        if capacity <= self.initial_buffer_size {
            return;
        }
        debug_assert!(
            capacity > self.initial_buffer_size,
            "shrink path only runs when capacity is above the initial floor"
        );
        if self.back_buf.available_data() == 0 {
            self.back_buf.shrink(self.initial_buffer_size);
            self.back_high_watermark_logged = false;
            // The back buffer is only shrunk once fully drained; it must end at
            // the floor with no pending data resurrected.
            debug_assert!(
                self.back_buf.capacity() >= self.initial_buffer_size,
                "back buffer must never shrink below the initial buffer size floor"
            );
            debug_assert_eq!(
                self.back_buf.available_data(),
                0,
                "back buffer must stay empty across a drained shrink"
            );
            trace!(
                "back buffer shrunk from {} to {} bytes",
                capacity, self.initial_buffer_size
            );
        }
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

    #[test]
    fn buffer_grows_with_doubling_strategy() {
        let (writing_channel, _reading_channel): (
            Channel<ProtobufMessage, ProtobufMessage>,
            Channel<ProtobufMessage, ProtobufMessage>,
        ) = Channel::generate(100, 10000).expect("could not generate channels");

        assert_eq!(writing_channel.back_buf.capacity(), 100);

        assert_eq!(writing_channel.grow_size(100), Some(200));
        assert_eq!(writing_channel.grow_size(200), Some(400));
        assert_eq!(writing_channel.grow_size(5000), Some(10000));
        assert_eq!(writing_channel.grow_size(10000), None);
    }

    #[test]
    fn buffer_cap_returns_error() {
        let (mut writing_channel, _reading_channel): (
            Channel<ProtobufMessage, ProtobufMessage>,
            Channel<ProtobufMessage, ProtobufMessage>,
        ) = Channel::generate(50, 50).expect("could not generate channels");

        writing_channel.blocking().expect("Could not block channel");

        let mut i = 0u32;
        let result = loop {
            let msg = ProtobufMessage { inner: i };
            match writing_channel.write_delimited_message(&msg) {
                Ok(()) => i += 1,
                Err(e) => break Err(e),
            }
            if i > 10000 {
                break Ok(());
            }
        };

        assert!(result.is_err());
        let err = result.unwrap_err();
        let err_msg = format!("{err}");
        assert!(
            err_msg.contains("too large") || err_msg.contains("cannot grow"),
            "unexpected error: {err_msg}"
        );
    }

    #[test]
    fn back_buffer_shrinks_after_drain() {
        let (mut channel, _other): (
            Channel<ProtobufMessage, ProtobufMessage>,
            Channel<ProtobufMessage, ProtobufMessage>,
        ) = Channel::generate(100, 10000).expect("could not generate channels");

        // Write directly to the back buffer (without draining to socket)
        // to force growth. Each message is ~10 bytes (delimiter + varint).
        for i in 0..20 {
            channel
                .write_delimited_message(&ProtobufMessage { inner: i })
                .expect("Could not write message");
        }

        let grown_capacity = channel.back_buf.capacity();
        assert!(
            grown_capacity > 100,
            "expected buffer growth, got capacity {grown_capacity}"
        );

        // Simulate full drain by consuming all data
        let data_len = channel.back_buf.available_data();
        channel.back_buf.consume(data_len);
        assert_eq!(channel.back_buf.available_data(), 0);

        channel.try_shrink_back_buf();
        assert_eq!(
            channel.back_buf.capacity(),
            100,
            "back buffer should shrink to initial size after drain"
        );
    }

    #[test]
    fn back_buffer_grows_with_doubling_on_write() {
        let (mut channel, _other): (
            Channel<ProtobufMessage, ProtobufMessage>,
            Channel<ProtobufMessage, ProtobufMessage>,
        ) = Channel::generate(32, 10000).expect("could not generate channels");

        assert_eq!(channel.back_buf.capacity(), 32);

        // Write enough messages to force growth beyond initial capacity.
        // Each ProtobufMessage encodes to ~4 bytes + 8-byte delimiter = ~12 bytes.
        for i in 0..10 {
            channel
                .write_delimited_message(&ProtobufMessage { inner: i })
                .expect("Could not write message");
        }

        let grown = channel.back_buf.capacity();
        assert!(grown > 32, "expected buffer growth beyond 32, got {grown}");
        // doubling from 32 should yield a power-of-two-like size (64, 128, 256, ...)
        // rather than the exact needed amount
        assert!(
            grown.is_power_of_two() || grown == 10000,
            "expected doubling growth pattern, got {grown}"
        );
    }

    /// Regression: a peer that writes a length-delimited frame whose
    /// declared length is *less than* the delimiter itself must be
    /// rejected with `MessageLengthUnderDelimiter`, never panic the
    /// reader with `slice index starts at N but ends at M`.
    ///
    /// Without the bounds check, `&buffer[delimiter_size()..message_len]`
    /// at `try_read_delimited_message` panics for any peer-controlled
    /// `message_len < delimiter_size()` (= 8 on 64-bit) — a one-packet
    /// denial-of-service against the master command socket.
    #[test]
    fn rejects_declared_length_below_delimiter() {
        let (mut reader, mut writer): (
            Channel<ProtobufMessage, ProtobufMessage>,
            Channel<ProtobufMessage, ProtobufMessage>,
        ) = Channel::generate(1000, 10000).expect("could not generate channels");
        writer.blocking().expect("writer to block");
        reader.blocking().expect("reader to block");

        // Craft a delimiter that lies: message_len = 5 (< delimiter_size() = 8).
        // Send it as raw bytes, bypassing write_delimited_message.
        let bogus: usize = 5;
        let bytes = bogus.to_le_bytes();
        std::io::Write::write_all(&mut writer.sock, &bytes).expect("raw write of bogus delimiter");

        match reader.read_message() {
            Err(ChannelError::MessageLengthUnderDelimiter {
                message_len,
                delimiter_size,
            }) => {
                assert_eq!(message_len, 5);
                assert_eq!(delimiter_size, std::mem::size_of::<usize>());
            }
            other => panic!(
                "expected MessageLengthUnderDelimiter, got {other:?}\n\
                 NOTE: a panic here means the slice-OOB hardening was reverted",
            ),
        }
    }

    /// Regression for sozu-proxy/sozu#1416: `Channel::new`/`generate_nonblocking` took
    /// `buffer_size` and `max_buffer_size` without ever comparing them, so a caller
    /// passing `buffer_size > max_buffer_size` built a `front_buf` already larger than
    /// the ceiling it must respect. `45, 32` is the exact pair the fuzz target's
    /// construction generator produced before it was clamped to work around this defect.
    /// `try_read_delimited_message` then tripped its own
    /// "front buffer capacity must never exceed max_buffer_size" `debug_assert!` on the
    /// very first parse, before any wire byte was examined.
    #[test]
    fn channel_new_clamps_buffer_size_above_max() {
        let (mut reader, _writer): (
            Channel<ProtobufMessage, ProtobufMessage>,
            Channel<ProtobufMessage, ProtobufMessage>,
        ) = Channel::generate_nonblocking(45, 32).expect("could not generate channels");

        // Parsing an empty buffer must not trip the capacity invariant assertion
        // (pre-fix: `try_read_delimited_message`'s own "front buffer capacity must
        // never exceed max_buffer_size" `debug_assert!` fires right here, before any
        // wire byte is examined).
        assert_eq!(
            reader
                .try_read_delimited_message()
                .expect("parsing an empty buffer must not error"),
            None
        );

        assert!(
            reader.front_buf.capacity() <= 32,
            "front buffer capacity {} must never exceed max_buffer_size 32",
            reader.front_buf.capacity()
        );
    }

    /// Regression for sozu-proxy/sozu#1428: a peer that declares a frame
    /// length above `max_buffer_size` must mark the channel for closing.
    ///
    /// The oversize guard rejects the frame but cannot re-sync past it. The
    /// declared length is *unsatisfiable* — the frame can never fit a buffer
    /// capped at `max_buffer_size`, so no amount of further reading completes
    /// it — and the bytes behind the delimiter are of unknown provenance:
    /// dropping the eight header bytes and re-framing would decode
    /// peer-chosen payload bytes as a fresh control-plane command. That is
    /// why this branch must NOT consume, while its sibling
    /// `MessageLengthUnderDelimiter` must: a declared length below
    /// `delimiter_size()` is one no writer can emit for any payload, so those
    /// eight bytes are provably not a real header and skipping exactly them
    /// re-aligns the stream.
    ///
    /// Left unconsumed AND unsignalled, the delimiter is re-parsed on every
    /// later read forever: `read_message_nonblocking` propagates the error
    /// with no log, `extract_messages` (`bin/src/command/sessions.rs`)
    /// discards it with a bare `Err(_)`, and neither `ClientSession::ready`
    /// nor `WorkerSession::ready` closes the session because both key on
    /// `readiness.is_error() || is_hup()`, which no parse error ever sets.
    /// The session then wedges permanently, pinning its file descriptor and
    /// up to `max_buffer_size` of buffer, and `doc/configure_admin_ops.md`
    /// §5.2's "the supervisor logs and drops the offending peer's session"
    /// never happens.
    #[test]
    fn oversized_declared_length_marks_the_channel_for_closing() {
        let (mut reader, mut writer): (
            Channel<ProtobufMessage, ProtobufMessage>,
            Channel<ProtobufMessage, ProtobufMessage>,
        ) = Channel::generate_nonblocking(1000, 10000)
            .expect("could not generate nonblocking channels");

        // One byte past this reader's `max_buffer_size`. Written raw only
        // because both ends of this pair share one ceiling, so the writer's own
        // `write_delimited_message` would refuse it. That is not true of a real
        // deployment: the two ends size their channels from their own
        // `max_command_buffer_size` (`bin/config.toml` ships 163_840,
        // `os-build/config.toml` 1_638_400, the compiled default is 2_000_000),
        // so a peer configured higher puts this frame on the wire through the
        // ordinary write path while conforming perfectly. What follows is
        // therefore about what this end can do with the frame, not about the
        // peer's good faith.
        let oversized: usize = 10_001;
        std::io::Write::write_all(&mut writer.sock, &oversized.to_le_bytes())
            .expect("raw write of the oversized delimiter");

        reader.handle_events(Ready::READABLE);
        reader
            .readable()
            .expect("reader must buffer the delimiter bytes");

        let available_before = reader.front_buf.available_data();
        assert_eq!(
            available_before,
            delimiter_size(),
            "the delimiter must be buffered before the parse under test"
        );

        // Arm writability before the parse that marks the channel, so the
        // assertion at the end of this test can tell an `insert` from an
        // assignment.
        reader.handle_events(Ready::WRITABLE);

        match reader.read_message() {
            Err(ChannelError::MessageTooLarge {
                message_len, max, ..
            }) => {
                assert_eq!(message_len, oversized);
                assert_eq!(max, 10000);
            }
            other => panic!("expected MessageTooLarge, got {other:?}"),
        }

        // The bytes stay put -- deliberately, see the doc comment above.
        assert_eq!(
            reader.front_buf.available_data(),
            available_before,
            "an unsatisfiable declared length must not be consumed: re-framing \
             on the bytes behind it would decode peer-chosen bytes as a command"
        );

        // So a second parse returns the identical error against the identical
        // bytes. The parser alone can never make progress on this stream.
        match reader.read_message() {
            Err(ChannelError::MessageTooLarge { message_len, .. }) => {
                assert_eq!(message_len, oversized)
            }
            other => panic!("expected the identical MessageTooLarge, got {other:?}"),
        }
        assert_eq!(reader.front_buf.available_data(), available_before);

        // Which leaves closing the peer as the only correct exit, and the channel
        // must say so through the one signal `ClientSession::ready`,
        // `WorkerSession::ready` and `wants_to_tick` all key on
        // (`bin/src/command/sessions.rs`).
        assert!(
            reader.readiness.is_error(),
            "an oversized declaration must mark the channel errored so the \
             supervisor drops the session; without it the same delimiter is \
             re-parsed forever, the error is swallowed by `extract_messages`'s \
             `Err(_)` arm, and the session wedges holding its fd and buffer"
        );
        // And it must INSERT that bit rather than assign it. On the worker's
        // own channel (`lib/src/server.rs`) `interest` carries only READABLE
        // and WRITABLE, so `readiness()` masks ERROR away and nothing there
        // reads `is_error()`: an assignment's only effect on that side is
        // clearing WRITABLE, and `Server::send_queue` gates on
        // `channel.readiness.is_writable()`, so responses already queued for
        // the supervisor would stall until the next edge-triggered writability
        // event.
        assert!(
            reader.readiness.is_writable(),
            "marking the channel errored must not clear the other readiness \
             bits; wiping WRITABLE stalls `Server::send_queue` until the next \
             writability event"
        );
    }

    /// Regression for the second shape of sozu-proxy/sozu#1428: a frame whose
    /// declared length is valid but whose payload does not decode must be
    /// consumed, so the channel re-syncs on the next frame instead of
    /// re-decoding the same bytes for the life of the session.
    ///
    /// `try_read_delimited_message` propagated `InvalidProtobufMessage` from
    /// the `?` on `Rx::decode`, one line *before* the `consume`. The malformed
    /// frame therefore stayed at the head of `front_buf`, no readiness bit
    /// moved, `extract_messages` (`bin/src/command/sessions.rs`) discarded the
    /// error with its bare `Err(_)` arm, and the session wedged exactly as the
    /// oversize branch above did. Eight valid length bytes followed by garbage
    /// reach it, and so does protobuf skew between two builds.
    ///
    /// Consuming is coherent with the success path rather than provably safe:
    /// `message_len` is only range-checked, but the success path already
    /// consumes that same peer-supplied length and re-frames on whatever
    /// follows, so the decode-failure path now re-frames on exactly the bytes
    /// the decode-success path already did. One undecodable payload therefore
    /// costs its frame rather than the peer.
    ///
    /// SCOPE: this asserts the channel contract, not a session one. Unlike the
    /// oversize branch above, this shape has no `ClientSession`-level test, so
    /// nothing here proves a session-level property. A frame pipelined behind
    /// the malformed one is delivered on the same tick; the session-level proof
    /// of that is
    /// `client_session_delivers_the_frame_behind_a_malformed_length_prefix`,
    /// built on the sibling `MessageLengthUnderDelimiter` shape.
    #[test]
    fn malformed_payload_is_consumed_so_the_channel_resyncs() {
        let (mut reader, mut writer): (
            Channel<ProtobufMessage, ProtobufMessage>,
            Channel<ProtobufMessage, ProtobufMessage>,
        ) = Channel::generate_nonblocking(1000, 10000)
            .expect("could not generate nonblocking channels");

        // A well-formed frame whose payload is not a `ProtobufMessage`: a
        // leading zero byte is field number 0, which no protobuf decoder
        // accepts. The length prefix itself is entirely valid. Hand-crafted
        // rather than random so the failure is deterministic and independent
        // of how much arbitrary input prost happens to accept.
        let frame_len: usize = delimiter_size() + 8;
        let mut malformed = frame_len.to_le_bytes().to_vec();
        malformed.extend_from_slice(&[0x00_u8; 8]);
        std::io::Write::write_all(&mut writer.sock, &malformed)
            .expect("raw write of the malformed frame");

        // ... immediately followed by a perfectly good one.
        writer
            .write_message(&ProtobufMessage { inner: 7 })
            .expect("could not queue the following message");
        writer.handle_events(Ready::WRITABLE);
        writer.run().expect("could not flush the following message");

        reader.handle_events(Ready::READABLE);
        reader.readable().expect("reader must buffer both frames");

        let available_before = reader.front_buf.available_data();
        assert!(
            available_before > frame_len,
            "both frames must be buffered before the parse under test"
        );

        match reader.read_message() {
            Err(ChannelError::InvalidProtobufMessage(_)) => {}
            other => panic!("expected InvalidProtobufMessage, got {other:?}"),
        }

        assert_eq!(
            reader.front_buf.available_data(),
            available_before - frame_len,
            "a malformed payload behind a valid length must be consumed: the \
             frame boundary is exact, so dropping exactly those bytes re-syncs \
             the stream"
        );

        assert_eq!(
            reader.read_message().expect("the next frame must decode"),
            ProtobufMessage { inner: 7 },
            "the channel must make progress past a malformed frame instead of \
             re-decoding it on every later read"
        );
    }

    /// Builds the exact front-buffer layout of sozu-proxy/sozu#1436 and returns
    /// the reader sitting in it, the writer still holding the pending frame's
    /// tail, and that tail.
    ///
    /// `Buffer::fill` (`command/src/buffer/growable.rs`) compacts whenever a
    /// read reaches the end of the buffer, so a front buffer filled to capacity
    /// always lands at `position == 0`. `Buffer::consume` compacts only past
    /// `capacity / 2`, so decoding one frame of at most half the capacity out of
    /// that full buffer advances `position` without reaching the threshold and
    /// leaves `available_space() == 0` with `capacity - first.len()` bytes
    /// pending. At `capacity == max_buffer_size` nothing can grow out of it
    /// either.
    ///
    /// `declared` is the length the second frame announces; the caller picks it
    /// on whichever side of the ceiling it wants to exercise.
    fn wedged_reader(
        capacity: usize,
        declared: usize,
    ) -> (
        Channel<ProtobufMessage, ProtobufMessage>,
        Channel<ProtobufMessage, ProtobufMessage>,
        Vec<u8>,
    ) {
        let (mut reader, mut writer): (
            Channel<ProtobufMessage, ProtobufMessage>,
            Channel<ProtobufMessage, ProtobufMessage>,
        ) = Channel::generate_nonblocking(capacity as u64, capacity as u64)
            .expect("could not generate nonblocking channels");

        // A real frame, produced by the production write path.
        writer
            .write_delimited_message(&ProtobufMessage { inner: 1 })
            .expect("could not frame the first message");
        let first = writer.back_buf.data().to_vec();
        writer.back_buf.consume(first.len());
        assert!(
            first.len() <= capacity / 2,
            "the first frame ({} bytes) must stay at or under `Buffer::consume`'s \
             {}-byte shift threshold, or the layout under test never forms",
            first.len(),
            capacity / 2
        );

        // The second frame. `[0x08, 0x2a]` is field 1 set to 42, repeated: a
        // non-repeated scalar takes its last occurrence, so the payload decodes
        // at any EVEN length -- callers pass an even `declared`, since an odd
        // one would truncate the last pair to a tag with no varint behind it.
        // Written raw because both ends of this pair share one ceiling, so
        // `write_delimited_message` cannot emit a frame this large -- a peer
        // configured with a larger `max_command_buffer_size` puts exactly these
        // bytes on the wire through the ordinary write path.
        let mut second = declared.to_le_bytes().to_vec();
        while second.len() < declared {
            second.extend_from_slice(&[0x08, 0x2a]);
        }
        debug_assert_eq!(
            declared % 2,
            0,
            "an odd declared length truncates the padding to a dangling tag"
        );
        second.truncate(declared.max(delimiter_size()));

        // Fill the front buffer to the brim: the first frame, then as much of
        // the second as fits. The rest stays in the writer.
        let head = capacity - first.len();
        let head = head.min(second.len());
        let mut wire = first.clone();
        wire.extend_from_slice(&second[..head]);
        std::io::Write::write_all(&mut writer.sock, &wire)
            .expect("raw write of the first frame and the second frame's head");

        reader.handle_events(Ready::READABLE);
        assert_eq!(
            reader
                .readable()
                .expect("the reader must fill its front buffer"),
            wire.len()
        );
        assert_eq!(
            reader.front_buf.available_space(),
            0,
            "the front buffer must be filled to the brim before the decode"
        );

        assert_eq!(
            reader.read_message().expect("the first frame must decode"),
            ProtobufMessage { inner: 1 }
        );
        assert_eq!(reader.front_buf.available_data(), capacity - first.len());
        assert_eq!(
            reader.front_buf.available_space(),
            0,
            "the layout under test: data pending, zero space, capacity pinned at \
             the ceiling -- `Buffer::consume` did not reach its shift threshold"
        );

        let tail = second[head..].to_vec();
        (reader, writer, tail)
    }

    /// Regression for sozu-proxy/sozu#1436: a frame that fits the ceiling but
    /// not the front buffer's current *layout* must be rescued by compaction,
    /// not rejected.
    ///
    /// The `BufferFull` arm of `try_read_delimited_message` returned without
    /// consuming and without moving a readiness bit -- the shape
    /// sozu-proxy/sozu#1428 removed from its two siblings -- and nothing
    /// downstream compensated: `extract_messages` (`bin/src/command/sessions.rs`)
    /// discards the error with a bare `Err(_)`, and `ClientSession::ready` /
    /// `WorkerSession::ready` close only on `readiness.is_error() || is_hup()`.
    /// Worse, it was unrecoverable: the three `front_buf` mutators that can
    /// trigger a `Buffer::shift` are all out of reach afterwards. `fill` needs a
    /// successful read, but `readable()` finds `available_space() == 0`, gets
    /// `None` from `grow_size` at the ceiling and executes
    /// `interest.remove(Ready::READABLE)`, so every later `readable()` returns
    /// `Err(Connection(None))`; `consume` needs a decoded frame or an
    /// under-delimiter length, and the parse returns before either;
    /// `try_shrink_front_buf` is only called after a frame decoded, and both
    /// of its own early returns would stop it anyway -- `capacity <=
    /// initial_buffer_size`, which holds whenever the channel was configured
    /// with `command_buffer_size == max_command_buffer_size`, and
    /// `available_data() * 4 < initial_buffer_size`, which is false with that
    /// much pending. Not because it shrinks rather than shifts: `Buffer::shrink`
    /// calls `shift` unconditionally before its own size bail.
    ///
    /// SCOPE: this is the channel contract. It proves the parser makes room and
    /// the frame decodes once its tail is buffered; it proves nothing about the
    /// session, whose drain loop had its own reason to stop one read short --
    /// see `client_session_completes_a_compacted_frame_without_another_peer_write`
    /// in `bin/src/command/sessions.rs`.
    ///
    /// The recovery is NOT the oversize branch's close. That branch drops the
    /// peer because the declared length can never be satisfied by a buffer
    /// bounded at `max_buffer_size`. Here the frame is under the ceiling and its
    /// buffered bytes are intact: only the buffer's internal offsets are in the
    /// way, and a single `Buffer::shift` hands back exactly the space the
    /// consumed frame left behind. Closing would drop a conforming peer over an
    /// internal buffer-management detail.
    #[test]
    fn a_frame_within_the_ceiling_is_rescued_by_compaction() {
        let capacity = 64usize;
        // Exactly at the ceiling: the largest frame this end may legitimately
        // be asked to accept, and the boundary case of the guard above.
        let (mut reader, mut writer, tail) = wedged_reader(capacity, capacity);
        assert!(
            !tail.is_empty(),
            "the second frame must still be incomplete"
        );

        std::io::Write::write_all(&mut writer.sock, &tail)
            .expect("raw write of the second frame's tail");

        // The parse that used to wedge. It must make room and ask for more
        // bytes, not declare the buffer unusable.
        match reader.read_message() {
            Err(ChannelError::NothingRead) => {}
            other => panic!(
                "expected the parser to compact and wait for the rest of the \
                 frame, got {other:?}"
            ),
        }
        assert!(
            reader.front_buf.available_space() >= tail.len(),
            "compaction must hand back the space the consumed frame left behind: \
             {} bytes of room for a {}-byte tail",
            reader.front_buf.available_space(),
            tail.len()
        );
        assert!(
            !reader.readiness.is_error(),
            "a frame within the ceiling must not mark the channel for closing"
        );

        // And the session is alive: it reads again, and the frame completes.
        reader.handle_events(Ready::READABLE);
        assert_eq!(
            reader
                .readable()
                .expect("the reader must still accept the frame's tail"),
            tail.len()
        );
        assert_eq!(
            reader
                .read_message()
                .expect("the second frame must decode once its tail is buffered"),
            ProtobufMessage { inner: 42 },
            "a conforming peer's frame must complete rather than wedge the session"
        );
    }

    /// The other side of sozu-proxy/sozu#1436's boundary: compaction must not
    /// rescue a frame that is genuinely unsatisfiable.
    ///
    /// A declared length above `max_buffer_size` does not fit a buffer bounded
    /// at `max_buffer_size` however it is compacted, so it must keep taking the
    /// sozu-proxy/sozu#1428 path -- `MessageTooLarge`, logged, channel marked
    /// errored -- from this layout exactly as from an empty buffer. The ceiling
    /// check runs before the zero-space arm precisely so that the two cases
    /// cannot be confused.
    #[test]
    fn a_frame_above_the_ceiling_still_closes_from_the_same_layout() {
        let capacity = 64usize;
        // Past the ceiling, in the very layout the compaction path rescues at
        // `capacity`. Even, because `wedged_reader` pads with two-byte protobuf
        // pairs and an odd length would leave a dangling tag; `capacity + 1` is
        // rejected by the identical guard, one byte earlier.
        let (mut reader, _writer, _tail) = wedged_reader(capacity, capacity + 2);

        let pending = reader.front_buf.available_data();
        match reader.read_message() {
            Err(ChannelError::MessageTooLarge {
                message_len, max, ..
            }) => {
                assert_eq!(message_len, capacity + 2);
                assert_eq!(max, capacity);
            }
            other => panic!(
                "expected MessageTooLarge: compaction must not rescue a frame the \
                 ceiling can never admit, got {other:?}"
            ),
        }
        assert_eq!(
            reader.front_buf.available_data(),
            pending,
            "an unsatisfiable declared length must still not be consumed"
        );
        assert!(
            reader.readiness.is_error(),
            "an unsatisfiable declared length must still mark the channel for \
             closing, whatever the buffer layout it was parsed from"
        );
    }

    /// A peer that dies with bytes still queued towards it makes the next
    /// `read(2)` fail with `ECONNRESET` rather than return EOF. That failure
    /// must leave the channel marked for closing exactly as EOF does: the
    /// supervisor's `WorkerSession::ready` closes a session on HUP or ERROR
    /// only, and an edge-triggered poller reports the hangup once, so a read
    /// error that wiped the readiness left a dead worker's session open for
    /// good (sozu-proxy/sozu#1560).
    #[test]
    fn a_read_error_keeps_the_channel_marked_for_closing() {
        for initial in [
            Ready::READABLE | Ready::WRITABLE | Ready::HUP | Ready::ERROR,
            Ready::READABLE | Ready::WRITABLE,
        ] {
            let (mut local, peer) =
                Channel::<ProtobufMessage, ProtobufMessage>::generate_nonblocking(1000, 10000)
                    .expect("could not generate nonblocking channels");
            // Leave a byte unread in the peer's receive queue, then close the
            // peer: the kernel resets the connection instead of sending EOF.
            assert_eq!(local.sock.write(b"x").expect("write to the live peer"), 1);
            drop(peer);

            local.readiness = initial;
            match local.readable() {
                Err(ChannelError::Read(error)) => {
                    assert_eq!(error.kind(), ErrorKind::ConnectionReset)
                }
                other => panic!("expected a connection reset, got {other:?}"),
            }
            assert!(
                local.readiness.is_hup() && local.readiness.is_error(),
                "a read error must leave HUP and ERROR set (from {initial:?}), got {:?}",
                local.readiness
            );
            assert!(!local.readiness.is_readable() && !local.readiness.is_writable());
            assert_eq!(local.interest, Ready::EMPTY);
        }
    }

    /// Same contract on the write side: `EPIPE` towards a closed peer must
    /// mark the channel for closing, as `Ok(0)` already does.
    #[test]
    fn a_write_error_keeps_the_channel_marked_for_closing() {
        for initial in [
            Ready::READABLE | Ready::WRITABLE | Ready::HUP | Ready::ERROR,
            Ready::READABLE | Ready::WRITABLE,
        ] {
            let (mut local, peer) =
                Channel::<ProtobufMessage, ProtobufMessage>::generate_nonblocking(1000, 10000)
                    .expect("could not generate nonblocking channels");
            drop(peer);

            local
                .write_delimited_message(&ProtobufMessage { inner: 7 })
                .expect("queue a message in the back buffer");
            local.interest.insert(Ready::WRITABLE);
            local.readiness = initial;
            match local.writable() {
                Err(ChannelError::Read(error)) => assert_eq!(error.kind(), ErrorKind::BrokenPipe),
                other => panic!("expected a broken pipe, got {other:?}"),
            }
            assert!(
                local.readiness.is_hup() && local.readiness.is_error(),
                "a write error must leave HUP and ERROR set (from {initial:?}), got {:?}",
                local.readiness
            );
            assert!(!local.readiness.is_readable() && !local.readiness.is_writable());
            assert_eq!(local.interest, Ready::EMPTY);
        }
    }
}
