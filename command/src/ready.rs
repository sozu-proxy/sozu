use std::fmt;

/// Binary representation of a file descriptor readiness (obtained through epoll)
#[derive(Copy, PartialEq, Eq, Clone, PartialOrd, Ord)]
pub struct Ready(pub u16);

impl Ready {
    pub const EMPTY: Ready = Ready(0);
    pub const READABLE: Ready = Ready(0b00001);
    pub const WRITABLE: Ready = Ready(0b00010);
    pub const ERROR: Ready = Ready(0b00100);
    /// Hang UP (see EPOLLHUP in epoll_ctl man page). Raised for either
    /// closed side, so a peer's half-close (`EPOLLRDHUP`) carries it too.
    pub const HUP: Ready = Ready(0b01000);
    /// The peer can receive nothing more: mio's `Event::is_write_closed`
    /// (`EPOLLHUP`, or `EPOLLERR`). Unlike HUP, a half-close (`EPOLLRDHUP`
    /// alone) never raises it. A reset whose error a `read` or `write`
    /// already consumed is reported as `EPOLLHUP` without `EPOLLERR`, so this
    /// bit, not ERROR, is what tells such a hang-up from a half-close.
    pub const WRITE_CLOSED: Ready = Ready(0b10000);
    pub const ALL: Ready = Ready(0b00011);

    #[inline]
    pub fn is_empty(&self) -> bool {
        self.0 == 0
    }

    #[inline]
    pub fn is_readable(&self) -> bool {
        self.contains(Ready::READABLE)
    }

    #[inline]
    pub fn is_writable(&self) -> bool {
        self.contains(Ready::WRITABLE)
    }

    pub fn is_error(&self) -> bool {
        self.contains(Ready::ERROR)
    }

    pub fn is_hup(&self) -> bool {
        self.contains(Ready::HUP)
    }

    pub fn is_write_closed(&self) -> bool {
        self.contains(Ready::WRITE_CLOSED)
    }

    #[inline]
    pub fn insert<T: Into<Self>>(&mut self, other: T) {
        let other = other.into();
        self.0 |= other.0;
    }

    #[inline]
    pub fn remove<T: Into<Self>>(&mut self, other: T) {
        let other = other.into();
        self.0 &= !other.0;
    }

    #[inline]
    pub fn contains<T: Into<Self>>(&self, other: T) -> bool {
        let other = other.into();
        (*self & other) == other
    }
}

//pub(crate) const RWINTEREST: mio::Interest = mio::Interest::READABLE | mio::Interest::WRITABLE;

use std::ops;
impl<T: Into<Ready>> ops::BitOr<T> for Ready {
    type Output = Ready;

    #[inline]
    fn bitor(self, other: T) -> Ready {
        Ready(self.0 | other.into().0)
    }
}

impl<T: Into<Ready>> ops::BitOrAssign<T> for Ready {
    #[inline]
    fn bitor_assign(&mut self, other: T) {
        self.0 |= other.into().0;
    }
}

impl<T: Into<Ready>> ops::BitAnd<T> for Ready {
    type Output = Ready;

    #[inline]
    fn bitand(self, other: T) -> Ready {
        Ready(self.0 & other.into().0)
    }
}

impl fmt::Debug for Ready {
    fn fmt(&self, fmt: &mut fmt::Formatter) -> fmt::Result {
        let mut one = false;
        let flags = [
            (Ready::READABLE, "Readable"),
            (Ready::WRITABLE, "Writable"),
            (Ready::ERROR, "Error"),
            (Ready::HUP, "Hup"),
            (Ready::WRITE_CLOSED, "WriteClosed"),
        ];

        for &(flag, msg) in &flags {
            if self.contains(flag) {
                if one {
                    write!(fmt, " | ")?
                }
                write!(fmt, "{msg}")?;

                one = true
            }
        }

        if !one {
            fmt.write_str("(empty)")?;
        }

        Ok(())
    }
}

impl std::convert::From<mio::Interest> for Ready {
    fn from(i: mio::Interest) -> Self {
        let mut r = Ready::EMPTY;
        if i.is_readable() {
            r.insert(Ready::READABLE);
        }
        if i.is_writable() {
            r.insert(Ready::WRITABLE);
        }

        r
    }
}

impl std::convert::From<&mio::event::Event> for Ready {
    fn from(e: &mio::event::Event) -> Self {
        let mut r = Ready::EMPTY;
        if e.is_readable() {
            r.insert(Ready::READABLE);
        }
        if e.is_writable() {
            r.insert(Ready::WRITABLE);
        }
        if e.is_error() {
            r.insert(Ready::ERROR);
        }
        if e.is_read_closed() || e.is_write_closed() {
            r.insert(Ready::HUP);
        }
        if e.is_write_closed() {
            r.insert(Ready::WRITE_CLOSED);
        }

        r
    }
}

// Real-epoll probes: the readiness words they check are Linux's.
#[cfg(all(test, target_os = "linux"))]
mod tests {
    use std::{
        io::{ErrorKind, Write},
        net::{Shutdown, TcpListener, TcpStream},
        os::fd::AsRawFd,
        time::Duration,
    };

    use mio::{Events, Interest, Poll, Token};

    use super::Ready;

    /// A connected pair: the client end, and the server end registered
    /// edge-triggered with a fresh `Poll`, its first edge already consumed.
    fn registered_pair() -> (TcpStream, mio::net::TcpStream, Poll, Events) {
        let listener = TcpListener::bind("127.0.0.1:0").expect("a loopback listener binds");
        let client =
            TcpStream::connect(listener.local_addr().expect("the listener has an address"))
                .expect("loopback connect completes");
        let (server, _) = listener.accept().expect("the connection is accepted");
        server
            .set_nonblocking(true)
            .expect("mio needs a nonblocking stream");
        let mut server = mio::net::TcpStream::from_std(server);
        let poll = Poll::new().expect("a poll opens");
        poll.registry()
            .register(
                &mut server,
                Token(1),
                Interest::READABLE | Interest::WRITABLE,
            )
            .expect("the stream registers");
        let mut events = Events::with_capacity(8);
        let mut poll = poll;
        poll.poll(&mut events, Some(Duration::from_millis(100)))
            .expect("the first poll succeeds");
        (client, server, poll, events)
    }

    /// Reset the connection: `SO_LINGER` zero, then close.
    fn reset(client: TcpStream) {
        let linger = libc::linger {
            l_onoff: 1,
            l_linger: 0,
        };
        // SAFETY: `linger` is a valid `libc::linger` and the descriptor is
        // owned by `client`, alive for the call.
        let rc = unsafe {
            libc::setsockopt(
                client.as_raw_fd(),
                libc::SOL_SOCKET,
                libc::SO_LINGER,
                &linger as *const _ as *const libc::c_void,
                std::mem::size_of::<libc::linger>() as libc::socklen_t,
            )
        };
        assert_eq!(rc, 0, "SO_LINGER zero must set");
        drop(client);
    }

    fn next_ready(poll: &mut Poll, events: &mut Events) -> Ready {
        poll.poll(events, Some(Duration::from_secs(2)))
            .expect("poll succeeds");
        let event = events
            .iter()
            .find(|event| event.token() == Token(1))
            .expect("the server end gets an event");
        Ready::from(event)
    }

    /// A half-close raises HUP, never WRITE_CLOSED: the peer still reads.
    #[test]
    fn a_half_close_is_hup_but_not_write_closed() {
        let (client, _server, mut poll, mut events) = registered_pair();
        client
            .shutdown(Shutdown::Write)
            .expect("the client half-closes");
        let ready = next_ready(&mut poll, &mut events);
        assert!(ready.is_hup(), "a half-close raises HUP: {ready:?}");
        assert!(
            !ready.is_write_closed(),
            "a half-close is not a hang-up: {ready:?}"
        );
        assert!(!ready.is_error(), "a half-close is not an error: {ready:?}");
    }

    /// A reset nothing has read yet is ERROR and WRITE_CLOSED.
    #[test]
    fn a_reset_is_error_and_write_closed() {
        let (client, _server, mut poll, mut events) = registered_pair();
        reset(client);
        let ready = next_ready(&mut poll, &mut events);
        assert!(ready.is_error(), "an unread reset is an error: {ready:?}");
        assert!(ready.is_write_closed(), "a reset is a hang-up: {ready:?}");
    }

    /// A reset whose error a `write` consumed first is reported as `EPOLLHUP`
    /// without `EPOLLERR`: HUP and WRITE_CLOSED, but not ERROR. ERROR alone
    /// therefore cannot tell this hang-up from a half-close; WRITE_CLOSED can.
    ///
    /// TO SEE THIS RED: drop the WRITE_CLOSED insertion from
    /// `Ready::from(&mio::event::Event)`.
    #[test]
    fn a_reset_consumed_by_a_write_is_write_closed_without_error() {
        let (client, mut server, mut poll, mut events) = registered_pair();
        reset(client);
        let mut consumed = None;
        for _ in 0..200 {
            match server.write_all(b"x") {
                Ok(_) => std::thread::sleep(Duration::from_millis(5)),
                Err(e) => {
                    consumed = Some(e.kind());
                    break;
                }
            }
        }
        assert!(
            matches!(
                consumed,
                Some(ErrorKind::ConnectionReset) | Some(ErrorKind::BrokenPipe)
            ),
            "premise: a write consumes the reset, got {consumed:?}"
        );
        let ready = next_ready(&mut poll, &mut events);
        assert!(ready.is_hup(), "the hang-up raises HUP: {ready:?}");
        assert!(
            ready.is_write_closed(),
            "the hang-up must be told from a half-close: {ready:?}"
        );
        assert!(
            !ready.is_error(),
            "premise: the write consumed the error, so EPOLLERR is gone: {ready:?}"
        );
    }
}
