use std::fmt;

/// Binary representation of a file descriptor readiness (obtained through epoll)
#[derive(Copy, PartialEq, Eq, Clone, PartialOrd, Ord)]
pub struct Ready(pub u16);

impl Ready {
    pub const EMPTY: Ready = Ready(0);
    pub const READABLE: Ready = Ready(0b00001);
    pub const WRITABLE: Ready = Ready(0b00010);
    pub const ERROR: Ready = Ready(0b00100);
    /// Hang UP (see EPOLLHUP in epoll_ctl man page)
    pub const HUP: Ready = Ready(0b01000);
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
        //FIXME: handle HUP events
        if e.is_read_closed() || e.is_write_closed() {
            r.insert(Ready::HUP);
        }

        r
    }
}
