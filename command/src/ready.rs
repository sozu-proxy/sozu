use std::fmt;

#[derive(Copy, PartialEq, Eq, Clone, PartialOrd, Ord)]
pub struct Ready(pub u16);

const READABLE: u16 = 0b00001;
const WRITABLE: u16 = 0b00010;
const ERROR: u16 = 0b00100;
const HUP: u16 = 0b01000;

impl Ready {
    pub fn empty() -> Ready {
        Ready(0)
    }

    #[inline]
    pub fn readable() -> Ready {
        Ready(READABLE)
    }

    #[inline]
    pub fn writable() -> Ready {
        Ready(WRITABLE)
    }

    #[inline]
    pub fn error() -> Ready {
        Ready(ERROR)
    }

    #[inline]
    pub fn hup() -> Ready {
        Ready(HUP)
    }

    #[inline]
    pub fn all() -> Ready {
        Ready(READABLE | WRITABLE)
    }

    #[inline]
    pub fn is_none(&self) -> bool {
        self.0 == 0
    }

    #[inline]
    pub fn is_readable(&self) -> bool {
        self.contains(Ready::readable())
    }

    #[inline]
    pub fn is_writable(&self) -> bool {
        self.contains(Ready::writable())
    }

    pub fn is_error(&self) -> bool {
        self.contains(Ready(ERROR))
    }

    pub fn is_hup(&self) -> bool {
        self.contains(Ready(HUP))
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
            (Ready::readable(), "Readable"),
            (Ready::writable(), "Writable"),
            (Ready(ERROR), "Error"),
            (Ready(HUP), "Hup")];

        for &(flag, msg) in &flags {
            if self.contains(flag) {
                if one { write!(fmt, " | ")? }
                write!(fmt, "{}", msg)?;

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
        let mut r = Ready::empty();
        if i.is_readable() {
            r.insert(Ready::readable());
        }
        if i.is_writable() {
            r.insert(Ready::writable());
        }

        r
    }
}

impl std::convert::From<&mio::event::Event> for Ready {
    fn from(e: &mio::event::Event) -> Self {
        let mut r = Ready::empty();
        if e.is_readable() {
            r.insert(Ready::readable());
        }
        if e.is_writable() {
            r.insert(Ready::writable());
        }
        if e.is_error() {
            r.insert(Ready::error());
        }
        //FIXME: handle HUP events

        r
    }
}
