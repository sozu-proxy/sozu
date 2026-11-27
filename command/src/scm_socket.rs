//! SCM_RIGHTS socket for FD passing between master and workers.
//!
//! Wraps a Unix stream and uses SCM_RIGHTS
//! to ship listener/accept FDs across the master ↔ worker boundary at
//! startup and across hot upgrades. The borrowed-FD wrappers
//! (`set_blocking`) hold an FD without taking ownership; the listener
//! teardown paths intentionally take ownership through
//! `TcpListener::from_raw_fd` so the FD is closed by drop.

use std::{
    io::IoSlice,
    mem::{self, MaybeUninit},
    os::unix::{
        io::{FromRawFd, IntoRawFd, OwnedFd, RawFd},
        net::UnixStream as StdUnixStream,
    },
    ptr,
};

use mio::net::{TcpListener, UdpSocket};
use nix::sys::socket;
use prost::{DecodeError, Message};

use crate::{
    listener_key::{ListenerKey, ListenerKeyError},
    proto::command::ListenersCount,
};

pub const MAX_FDS_OUT: usize = 200;
pub const MAX_BYTES_OUT: usize = 4096;

// Linux accepts at most 253 descriptors in one SCM_RIGHTS message. Receiving
// that full kernel limit lets us take ownership before rejecting a frame that
// exceeds Sōzu's stricter MAX_FDS_OUT contract.
const MAX_FDS_IN_CONTROL_MESSAGE: usize = 253;
const CONTROL_MESSAGE_BUFFER_SIZE: usize =
    socket::cmsg_space::<[RawFd; MAX_FDS_IN_CONTROL_MESSAGE]>();

#[repr(C)]
struct AlignedControlBuffer {
    _alignment: [MaybeUninit<libc::cmsghdr>; 0],
    bytes: [u8; CONTROL_MESSAGE_BUFFER_SIZE],
}

#[derive(thiserror::Error, Debug)]
pub enum ScmSocketError {
    #[error("could not set the blocking status of the unix stream to {blocking}: {error}")]
    SetBlocking {
        blocking: bool,
        error: std::io::Error,
    },
    #[error("could not send message for SCM socket: {0}")]
    Send(String),
    #[error("could not receive message for SCM socket: {0}")]
    Receive(String),
    #[error("invalid char set: {0}")]
    InvalidCharSet(String),
    #[error("Could not deserialize utf8 string into listeners: {0}")]
    ListenerParse(String),
    #[error("Wrong listener key {address}: {error}")]
    WrongSocketAddress {
        address: String,
        error: ListenerKeyError,
    },
    #[error("error decoding the protobuf format of the listeners: {0}")]
    DecodeError(DecodeError),
    #[error(
        "listeners count manifest is inconsistent with the SCM payload: \
         http={http}, tls={tls}, tcp={tcp} (sum={total}), fds_received={fds_received}, max_fds={max_fds}"
    )]
    ListenersCountInconsistent {
        http: usize,
        tls: usize,
        tcp: usize,
        total: usize,
        fds_received: usize,
        max_fds: usize,
    },
    #[error(
        "listener batch does not fit one SCM frame: {fds} file descriptors (max {max_fds}), \
         {bytes} encoded bytes (max {max_bytes})"
    )]
    ListenerBatchTooLarge {
        fds: usize,
        max_fds: usize,
        bytes: usize,
        max_bytes: usize,
    },
    #[error("SCM socket sent only {sent} of {expected} message bytes")]
    SendIncomplete { sent: usize, expected: usize },
    #[error("received a truncated SCM frame (data={data}, control={control})")]
    ReceiveTruncated { data: bool, control: bool },
}

struct ListenerBatch {
    message: Vec<u8>,
    file_descriptors: Vec<RawFd>,
}

fn build_listener_batch(listeners: &Listeners) -> Result<ListenerBatch, ScmSocketError> {
    let listeners_count = ListenersCount {
        http: listeners.http.iter().map(|t| t.0.to_string()).collect(),
        tls: listeners.tls.iter().map(|t| t.0.to_string()).collect(),
        tcp: listeners.tcp.iter().map(|t| t.0.to_string()).collect(),
        udp: listeners.udp.iter().map(|t| t.0.to_string()).collect(),
    };

    debug_assert_eq!(listeners_count.http.len(), listeners.http.len());
    debug_assert_eq!(listeners_count.tls.len(), listeners.tls.len());
    debug_assert_eq!(listeners_count.tcp.len(), listeners.tcp.len());
    debug_assert_eq!(listeners_count.udp.len(), listeners.udp.len());

    let message = listeners_count.encode_length_delimited_to_vec();
    let file_descriptors = listeners
        .http
        .iter()
        .chain(&listeners.tls)
        .chain(&listeners.tcp)
        .chain(&listeners.udp)
        .map(|listener| listener.1)
        .collect::<Vec<_>>();

    let address_total =
        listeners.http.len() + listeners.tls.len() + listeners.tcp.len() + listeners.udp.len();
    debug_assert_eq!(file_descriptors.len(), address_total);

    if file_descriptors.len() > MAX_FDS_OUT || message.len() > MAX_BYTES_OUT {
        return Err(ScmSocketError::ListenerBatchTooLarge {
            fds: file_descriptors.len(),
            max_fds: MAX_FDS_OUT,
            bytes: message.len(),
            max_bytes: MAX_BYTES_OUT,
        });
    }

    Ok(ListenerBatch {
        message,
        file_descriptors,
    })
}

/// Validate that listeners fit the exact descriptor and encoded-byte limits
/// of one SCM transport frame.
pub fn validate_listener_batch(listeners: &Listeners) -> Result<(), ScmSocketError> {
    build_listener_batch(listeners).map(|_| ())
}

/// A unix socket specialized for file descriptor passing
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct ScmSocket {
    pub fd: RawFd,
    pub blocking: bool,
}

impl ScmSocket {
    /// Create a blocking SCM socket from a raw file descriptor (unsafe)
    pub fn new(fd: RawFd) -> Result<Self, ScmSocketError> {
        // SAFETY: `fd` is borrowed for the duration of this block. We wrap it
        // in a `StdUnixStream` to call `set_nonblocking`, then immediately
        // release ownership again with `into_raw_fd` so the descriptor is
        // not closed by `Drop`. The caller retains ownership of `fd`.
        unsafe {
            let stream = StdUnixStream::from_raw_fd(fd);
            stream
                .set_nonblocking(false)
                .map_err(|error| ScmSocketError::SetBlocking {
                    blocking: false,
                    error,
                })?;
            let _dropped_fd = stream.into_raw_fd();
        }

        Ok(ScmSocket { fd, blocking: true })
    }

    /// Get the raw file descriptor of the scm channel
    pub fn raw_fd(&self) -> i32 {
        self.fd
    }

    /// Use the standard library (unsafe) to set the socket to blocking / unblocking
    pub fn set_blocking(&mut self, blocking: bool) -> Result<(), ScmSocketError> {
        if self.blocking == blocking {
            return Ok(());
        }
        // Past the idempotent early-return the flag must actually be flipping.
        debug_assert_ne!(
            self.blocking, blocking,
            "set_blocking only reaches the syscall when the state actually changes"
        );
        let blocking_before = self.blocking;
        // SAFETY: `self.fd` is borrowed for the duration of this block. We wrap
        // it in a `StdUnixStream` to call `set_nonblocking`, then immediately
        // release ownership with `into_raw_fd` so the descriptor is not closed
        // by `Drop`. `ScmSocket` retains the original ownership.
        unsafe {
            let stream = StdUnixStream::from_raw_fd(self.fd);
            let result = stream.set_nonblocking(!blocking);
            let _dropped_fd = stream.into_raw_fd();
            result.map_err(|error| ScmSocketError::SetBlocking { blocking, error })?;
        }
        self.blocking = blocking;
        // The flag landed on the requested value and genuinely toggled.
        debug_assert_eq!(
            self.blocking, blocking,
            "blocking flag must reflect the requested state after a successful set"
        );
        debug_assert_ne!(
            self.blocking, blocking_before,
            "blocking flag must have toggled across a real state change"
        );
        Ok(())
    }

    /// Send listeners (socket addresses and file descriptors) via an scm socket
    pub fn send_listeners(&self, listeners: &Listeners) -> Result<(), ScmSocketError> {
        let batch = build_listener_batch(listeners)?;
        self.send_msg_and_fds(&batch.message, &batch.file_descriptors)
    }

    /// Receive and parse listeners (socket addresses and file descriptors) via an scm socket
    pub fn receive_listeners(&self) -> Result<Listeners, ScmSocketError> {
        let mut buf = vec![0; MAX_BYTES_OUT];
        let (size, received_fds) = self.receive_msg_and_fds(&mut buf)?;
        let file_descriptor_length = received_fds.len();

        debug!("{} received :{:?}", self.fd, (size, file_descriptor_length));

        let listeners_count = ListenersCount::decode_length_delimited(&buf[..size])
            .map_err(ScmSocketError::DecodeError)?;

        // Validate the manifest before indexing into the owned FD vector.
        // The peer-controlled `listeners_count.{http,tls,tcp,udp}` lists are
        // matched 1:1 with `received_fds` slots; without these bounds checks
        // a peer that declared more entries than MAX_FDS_OUT or more entries
        // than FDs actually arrived would panic the worker on
        // `received_fds[index..index + len]`. `udp` is folded into the total
        // and the inconsistency-error `tcp` slot for diagnostics rather than
        // widening the error variant.
        let http_len = listeners_count.http.len();
        let tls_len = listeners_count.tls.len();
        let tcp_len = listeners_count.tcp.len();
        let udp_len = listeners_count.udp.len();
        let total = http_len
            .checked_add(tls_len)
            .and_then(|s| s.checked_add(tcp_len))
            .and_then(|s| s.checked_add(udp_len))
            .ok_or(ScmSocketError::ListenersCountInconsistent {
                http: http_len,
                tls: tls_len,
                tcp: tcp_len.saturating_add(udp_len),
                total: usize::MAX,
                fds_received: file_descriptor_length,
                max_fds: MAX_FDS_OUT,
            })?;
        if total > MAX_FDS_OUT || total != file_descriptor_length {
            return Err(ScmSocketError::ListenersCountInconsistent {
                http: http_len,
                tls: tls_len,
                tcp: tcp_len.saturating_add(udp_len),
                total,
                fds_received: file_descriptor_length,
                max_fds: MAX_FDS_OUT,
            });
        }

        // Past the consistency guard, the folded total reconciles with both the
        // transport bound and the number of FDs that actually arrived.
        // These are the invariants that keep every `received_fds[index..index+len]`
        // slice below in bounds — a malformed manifest already returned an error
        // and never reaches here.
        debug_assert_eq!(
            total,
            http_len + tls_len + tcp_len + udp_len,
            "folded total must equal the sum of per-protocol counts"
        );
        debug_assert!(
            total <= MAX_FDS_OUT,
            "total FD slots must fit the transport limit before indexing"
        );
        debug_assert_eq!(
            total, file_descriptor_length,
            "manifest total must equal the FDs actually received"
        );
        debug_assert!(
            total <= received_fds.len(),
            "every (address, fd) zip below must stay within the received_fds array"
        );

        let mut http_addresses = parse_addresses(&listeners_count.http)?;
        let mut tls_addresses = parse_addresses(&listeners_count.tls)?;
        let mut tcp_addresses = parse_addresses(&listeners_count.tcp)?;
        let mut udp_addresses = parse_addresses(&listeners_count.udp)?;

        // All fallible decode, consistency, and address validation is complete.
        // Only now relinquish RAII ownership to the successful Listeners table.
        let received_fds = received_fds
            .into_iter()
            .map(IntoRawFd::into_raw_fd)
            .collect::<Vec<_>>();

        // Each parsed address list maps 1:1 onto a contiguous FD slice; the
        // counts must survive `parse_addresses` unchanged.
        debug_assert_eq!(
            http_addresses.len(),
            http_len,
            "parsed http address count must match the manifest count"
        );
        debug_assert_eq!(
            tls_addresses.len(),
            tls_len,
            "parsed tls address count must match the manifest count"
        );
        debug_assert_eq!(
            tcp_addresses.len(),
            tcp_len,
            "parsed tcp address count must match the manifest count"
        );

        let mut index = 0;
        let len = http_len;
        // Each FD slice end must stay within the validated total; pair-assert
        // the slice window before every zip.
        debug_assert!(
            index + len <= total,
            "http FD slice must lie within the reconciled total"
        );
        let mut http = Vec::new();
        http.extend(
            http_addresses
                .drain(..)
                .zip(received_fds[index..index + len].iter().cloned()),
        );
        // Each address was wrapped with exactly one FD.
        debug_assert_eq!(
            http.len(),
            http_len,
            "every http address must be paired with exactly one FD"
        );

        index += len;
        let len = tls_len;
        debug_assert!(
            index + len <= total,
            "tls FD slice must lie within the reconciled total"
        );
        let mut tls = Vec::new();
        tls.extend(
            tls_addresses
                .drain(..)
                .zip(received_fds[index..index + len].iter().cloned()),
        );
        debug_assert_eq!(
            tls.len(),
            tls_len,
            "every tls address must be paired with exactly one FD"
        );

        index += len;
        let len = tcp_len;
        debug_assert!(
            index + len <= total,
            "tcp FD slice must lie within the reconciled total"
        );
        let mut tcp = Vec::new();
        tcp.extend(
            tcp_addresses
                .drain(..)
                .zip(received_fds[index..index + len].iter().cloned()),
        );
        debug_assert_eq!(
            tcp.len(),
            tcp_len,
            "every tcp address must be paired with exactly one FD"
        );

        index += len;
        let len = udp_len;
        let mut udp = Vec::new();
        udp.extend(
            udp_addresses
                .drain(..)
                .zip(received_fds[index..index + len].iter().cloned()),
        );
        debug_assert_eq!(
            udp.len(),
            udp_len,
            "every udp address must be paired with exactly one FD"
        );

        // The reconstructed tables consume every FD slot the manifest declared:
        // the final cursor lands exactly on the folded total (http+tls+tcp+udp).
        debug_assert_eq!(
            index + len,
            total,
            "the (address, fd) zips must consume exactly the reconciled total of FD slots"
        );
        debug_assert_eq!(
            http.len() + tls.len() + tcp.len() + udp.len(),
            total,
            "reconstructed listener count must equal the reconciled FD total"
        );

        Ok(Listeners {
            http,
            tls,
            tcp,
            udp,
        })
    }

    /// Best-effort removal of one terminal listener transfer from the stream.
    /// Every delivered descriptor is immediately closed by `OwnedFd` drop.
    pub fn discard_pending_listener_transfer(&self) {
        let mut message = [0; MAX_BYTES_OUT];
        let mut control = AlignedControlBuffer {
            _alignment: [],
            bytes: [0; CONTROL_MESSAGE_BUFFER_SIZE],
        };
        let _ = self.receive_msg_and_fds_with_control(&mut message, &mut control.bytes, true);
    }

    /// Sends message and file descriptors separately. The file descriptors are summed up
    /// in a ControlMessage.
    fn send_msg_and_fds(&self, message: &[u8], fds: &[RawFd]) -> Result<(), ScmSocketError> {
        let iov = [IoSlice::new(message)];
        let flags = if self.blocking {
            socket::MsgFlags::empty()
        } else {
            socket::MsgFlags::MSG_DONTWAIT
        };

        if fds.is_empty() {
            debug!("{} send empty", self.fd);
            let sent = socket::sendmsg::<()>(self.fd, &iov, &[], flags, None)
                .map_err(|error| ScmSocketError::Send(error.to_string()))?;
            if sent != message.len() {
                return Err(ScmSocketError::SendIncomplete {
                    sent,
                    expected: message.len(),
                });
            }
            return Ok(());
        };

        let control_message = [socket::ControlMessage::ScmRights(fds)];
        debug!("{} send with data", self.fd);
        let sent = socket::sendmsg::<()>(self.fd, &iov, &control_message, flags, None)
            .map_err(|error| ScmSocketError::Send(error.to_string()))?;
        if sent != message.len() {
            return Err(ScmSocketError::SendIncomplete {
                sent,
                expected: message.len(),
            });
        }
        Ok(())
    }

    /// Parse the message and receives file descriptors separately via the ControlMessage
    fn receive_msg_and_fds(
        &self,
        message: &mut [u8],
    ) -> Result<(usize, Vec<OwnedFd>), ScmSocketError> {
        let mut control = AlignedControlBuffer {
            _alignment: [],
            bytes: [0; CONTROL_MESSAGE_BUFFER_SIZE],
        };
        self.receive_msg_and_fds_with_control(message, &mut control.bytes, false)
    }

    fn receive_msg_and_fds_with_control(
        &self,
        message: &mut [u8],
        control: &mut [u8],
        force_nonblocking: bool,
    ) -> Result<(usize, Vec<OwnedFd>), ScmSocketError> {
        let flags = if self.blocking && !force_nonblocking {
            socket::MsgFlags::empty()
        } else {
            socket::MsgFlags::MSG_DONTWAIT
        };

        // Received descriptors are close-on-exec. The main process forks and
        // execs a new worker while it holds the listeners it just received
        // from the retiring one; without the flag the new worker inherits a
        // second, untracked copy of every listener, which keeps the socket
        // listening after the worker deactivates it. The paths that do hand a
        // descriptor across `exec` clear the flag explicitly on that
        // descriptor (`util::disable_close_on_exec` in `bin/`), and none of
        // them hands over a received listener. `MSG_CMSG_CLOEXEC` sets the
        // flag atomically with the receipt; where it does not exist, the flag
        // is set right after, which leaves a window for a concurrent `fork`.
        #[cfg(any(
            target_os = "linux",
            target_os = "android",
            target_os = "freebsd",
            target_os = "dragonfly",
            target_os = "netbsd",
            target_os = "openbsd"
        ))]
        let flags = flags | socket::MsgFlags::MSG_CMSG_CLOEXEC;

        let mut iov = libc::iovec {
            iov_base: message.as_mut_ptr().cast(),
            iov_len: message.len(),
        };
        let mut header: libc::msghdr = unsafe { mem::zeroed() };
        header.msg_iov = &mut iov;
        header.msg_iovlen = 1;
        header.msg_control = control.as_mut_ptr().cast();
        // msg_controllen is usize on Linux and socklen_t on BSD-family targets.
        let control_len = u16::try_from(control.len()).expect("the ancillary buffer fits in u16");
        header.msg_controllen = control_len.into();

        // SAFETY: header points to the live message and aligned control buffers
        // above for their full lengths. recvmsg initializes only those buffers
        // and the scalar result fields in header.
        let received = unsafe { libc::recvmsg(self.fd, &mut header, flags.bits()) };
        if received < 0 {
            return Err(ScmSocketError::Receive(
                std::io::Error::last_os_error().to_string(),
            ));
        }

        let received_fds = own_scm_rights(&header)?;
        let data_truncated = header.msg_flags & libc::MSG_TRUNC != 0;
        let control_truncated = header.msg_flags & libc::MSG_CTRUNC != 0;
        if data_truncated || control_truncated {
            // received_fds owns every descriptor described by the delivered
            // control prefix, including when descriptor limits caused CTRUNC.
            return Err(ScmSocketError::ReceiveTruncated {
                data: data_truncated,
                control: control_truncated,
            });
        }

        debug_assert!((received as usize) <= message.len());
        Ok((received as usize, received_fds))
    }
}

fn own_scm_rights(header: &libc::msghdr) -> Result<Vec<OwnedFd>, ScmSocketError> {
    let control_start = header.msg_control as usize;
    // recvmsg cannot report more control bytes than the buffer supplied.
    let control_len = u16::try_from(header.msg_controllen)
        .map(usize::from)
        .map_err(|_| ScmSocketError::Receive("invalid ancillary data length".to_owned()))?;
    let control_end = control_start
        .checked_add(control_len)
        .ok_or_else(|| ScmSocketError::Receive("invalid ancillary data length".to_owned()))?;
    let minimum_header_len = unsafe { libc::CMSG_LEN(0) as usize };
    let mut owned_fds = Vec::new();

    // nix 0.32 refuses RecvMsg::cmsgs() when MSG_CTRUNC is set. Walking the
    // kernel-produced prefix directly is necessary to own and close the FDs
    // installed before descriptor limits truncated the message.
    // SAFETY: header comes directly from a successful recvmsg call. Every
    // dereference and payload read below is first bounded by msg_controllen.
    let mut current = unsafe { libc::CMSG_FIRSTHDR(header) };
    while !current.is_null() {
        let current_address = current as usize;
        let remaining = control_end.checked_sub(current_address).ok_or_else(|| {
            ScmSocketError::Receive("ancillary header lies outside its buffer".to_owned())
        })?;
        if current_address < control_start || remaining < mem::size_of::<libc::cmsghdr>() {
            return Err(ScmSocketError::Receive(
                "truncated ancillary header".to_owned(),
            ));
        }

        // SAFETY: current is aligned by CMSG_FIRSTHDR/CMSG_NXTHDR and the full
        // cmsghdr was checked to lie inside the control buffer.
        let cmsg = unsafe { ptr::read(current) };
        let cmsg_len = cmsg.cmsg_len as usize;
        if cmsg_len < minimum_header_len || cmsg_len > remaining {
            return Err(ScmSocketError::Receive(
                "invalid ancillary message length".to_owned(),
            ));
        }

        // SAFETY: current points to a complete cmsghdr validated above.
        let data = unsafe { libc::CMSG_DATA(current) };
        let data_offset = (data as usize)
            .checked_sub(current_address)
            .ok_or_else(|| {
                ScmSocketError::Receive("invalid ancillary payload pointer".to_owned())
            })?;
        if data_offset > cmsg_len {
            return Err(ScmSocketError::Receive(
                "ancillary payload lies outside its message".to_owned(),
            ));
        }

        if cmsg.cmsg_level == libc::SOL_SOCKET && cmsg.cmsg_type == libc::SCM_RIGHTS {
            let payload_len = cmsg_len - data_offset;
            if !payload_len.is_multiple_of(mem::size_of::<RawFd>()) {
                return Err(ScmSocketError::Receive(
                    "invalid SCM_RIGHTS payload length".to_owned(),
                ));
            }

            for index in 0..payload_len / mem::size_of::<RawFd>() {
                // SAFETY: the payload bounds and RawFd-sized divisibility were
                // checked above. read_unaligned handles CMSG_DATA alignment.
                let fd = unsafe { ptr::read_unaligned(data.cast::<RawFd>().add(index)) };
                if fd < 0 {
                    return Err(ScmSocketError::Receive(
                        "received an invalid file descriptor".to_owned(),
                    ));
                }
                // SAFETY: recvmsg installed this descriptor in the process and
                // SCM_RIGHTS transfers ownership to the receiver exactly once.
                let owned_fd = unsafe { OwnedFd::from_raw_fd(fd) };
                #[cfg(not(any(
                    target_os = "linux",
                    target_os = "android",
                    target_os = "freebsd",
                    target_os = "dragonfly",
                    target_os = "netbsd",
                    target_os = "openbsd"
                )))]
                // SAFETY: owned_fd is live and F_SETFD changes only its flags.
                unsafe {
                    libc::fcntl(fd, libc::F_SETFD, libc::FD_CLOEXEC);
                }
                owned_fds.push(owned_fd);
            }
        }

        // SAFETY: current is a validated header from this msghdr. libc applies
        // the platform's CMSG alignment and returns null at msg_controllen.
        current = unsafe { libc::CMSG_NXTHDR(header, current) };
    }

    Ok(owned_fds)
}

/// Listener keys (socket address, plus interface when bound) and file
/// descriptors of listening sockets, needed by a
/// Proxy to start listening. The transport is fd-type-agnostic: `udp` carries
/// `UdpSocket` fds, the others carry `TcpListener` fds.
#[derive(Clone, Default, Debug, Serialize, Deserialize, PartialEq)]
pub struct Listeners {
    pub http: Vec<(ListenerKey, RawFd)>,
    pub tls: Vec<(ListenerKey, RawFd)>,
    pub tcp: Vec<(ListenerKey, RawFd)>,
    #[serde(default)]
    pub udp: Vec<(ListenerKey, RawFd)>,
}

impl Listeners {
    pub fn get_http(&mut self, addr: &ListenerKey) -> Option<RawFd> {
        let before = self.http.len();
        let pos = self.http.iter().position(|(front, _)| front == addr);
        let result = pos.map(|pos| self.http.remove(pos).1);
        // Exactly the matched entry is removed: the length drops by one iff a
        // match was found, and the removed address is truly gone.
        debug_assert_eq!(
            self.http.len(),
            before - result.is_some() as usize,
            "http listener table shrinks by exactly one iff an address matched"
        );
        debug_assert!(
            result.is_none() || !self.http.iter().any(|(front, _)| front == addr),
            "the matched http address must no longer be present after removal"
        );
        result
    }

    pub fn get_https(&mut self, addr: &ListenerKey) -> Option<RawFd> {
        let before = self.tls.len();
        let pos = self.tls.iter().position(|(front, _)| front == addr);
        let result = pos.map(|pos| self.tls.remove(pos).1);
        debug_assert_eq!(
            self.tls.len(),
            before - result.is_some() as usize,
            "tls listener table shrinks by exactly one iff an address matched"
        );
        debug_assert!(
            result.is_none() || !self.tls.iter().any(|(front, _)| front == addr),
            "the matched tls address must no longer be present after removal"
        );
        result
    }

    pub fn get_tcp(&mut self, addr: &ListenerKey) -> Option<RawFd> {
        let before = self.tcp.len();
        let pos = self.tcp.iter().position(|(front, _)| front == addr);
        let result = pos.map(|pos| self.tcp.remove(pos).1);
        debug_assert_eq!(
            self.tcp.len(),
            before - result.is_some() as usize,
            "tcp listener table shrinks by exactly one iff an address matched"
        );
        debug_assert!(
            result.is_none() || !self.tcp.iter().any(|(front, _)| front == addr),
            "the matched tcp address must no longer be present after removal"
        );
        result
    }

    pub fn get_udp(&mut self, addr: &ListenerKey) -> Option<RawFd> {
        let before = self.udp.len();
        let pos = self.udp.iter().position(|(front, _)| front == addr);
        let result = pos.map(|pos| self.udp.remove(pos).1);
        debug_assert_eq!(
            self.udp.len(),
            before - result.is_some() as usize,
            "udp listener table shrinks by exactly one iff an address matched"
        );
        debug_assert!(
            result.is_none() || !self.udp.iter().any(|(front, _)| front == addr),
            "the matched udp address must no longer be present after removal"
        );
        result
    }

    /// Deactivate all listeners by closing their file descriptors
    pub fn close(&self) {
        for (_, fd) in &self.http {
            // SAFETY: `*fd` is owned by this `ScmListeners` table and is
            // about to be closed by the binding's `Drop` (intentional
            // close-by-drop). No other reference to the descriptor survives.
            unsafe {
                let _ = TcpListener::from_raw_fd(*fd);
            }
        }

        for (_, fd) in &self.tls {
            // SAFETY: `*fd` is owned by this `ScmListeners` table and is
            // about to be closed by the binding's `Drop` (intentional
            // close-by-drop). No other reference to the descriptor survives.
            unsafe {
                let _ = TcpListener::from_raw_fd(*fd);
            }
        }

        for (_, fd) in &self.tcp {
            // SAFETY: `*fd` is owned by this `ScmListeners` table and is
            // about to be closed by the binding's `Drop` (intentional
            // close-by-drop). No other reference to the descriptor survives.
            unsafe {
                let _ = TcpListener::from_raw_fd(*fd);
            }
        }

        for (_, fd) in &self.udp {
            // SAFETY: `*fd` is owned by this `ScmListeners` table and is
            // about to be closed by the binding's `Drop` (intentional
            // close-by-drop). No other reference to the descriptor survives.
            // UDP listeners are `UdpSocket` fds, so take ownership through the
            // matching wrapper.
            unsafe {
                let _ = UdpSocket::from_raw_fd(*fd);
            }
        }
    }
}

/// Parse the manifest's listener keys: the socket address, followed by
/// `%<interface>` for a listener bound to a network interface. A manifest from
/// a Sōzu that predates interfaces carries bare addresses, which parse as keys
/// without an interface.
fn parse_addresses(addresses: &[String]) -> Result<Vec<ListenerKey>, ScmSocketError> {
    let mut parsed_addresses = Vec::new();
    for address in addresses {
        parsed_addresses.push(address.parse::<ListenerKey>().map_err(|error| {
            ScmSocketError::WrongSocketAddress {
                address: address.to_owned(),
                error,
            }
        })?);
    }
    Ok(parsed_addresses)
}

#[cfg(test)]
mod tests {

    use std::{
        io::Write,
        net::SocketAddr,
        os::unix::{net::UnixStream, prelude::AsRawFd},
        str::FromStr,
    };

    use mio::net::UnixStream as MioUnixStream;

    use super::*;

    #[test]
    fn create_block_unblock_an_scm_socket() {
        let (nonblocking_stream, _) =
            MioUnixStream::pair().expect("Could not create a pair of unix streams");
        let raw_file_descriptor = nonblocking_stream.into_raw_fd();

        let scm_socket = ScmSocket::new(raw_file_descriptor);
        assert!(scm_socket.is_ok());

        let mut scm_socket = scm_socket.unwrap();

        assert!(scm_socket.set_blocking(true).is_ok());
        assert!(scm_socket.set_blocking(false).is_ok());
    }

    fn socket_addr_from_str(str: &str) -> SocketAddr {
        SocketAddr::from_str(str)
            .unwrap_or_else(|_| panic!("failed to create socket address from string slice {str}"))
    }

    /// A descriptor received over the SCM socket must be close-on-exec.
    ///
    /// Without `MSG_CMSG_CLOEXEC` the kernel installs it without
    /// `FD_CLOEXEC`, so every process the receiver later spawns with `exec`
    /// inherits the listening socket: the main process forks and execs a new
    /// worker while it holds the listeners it just received from the retiring
    /// one, and that worker ends up holding each listener twice, once through
    /// an fd it never tracks. When the worker later deactivates the listener,
    /// the untracked copy keeps it listening.
    #[test]
    fn received_listener_descriptors_are_close_on_exec() {
        let (stream_1, stream_2) =
            MioUnixStream::pair().expect("Could not create a pair of mio unix streams");
        let sending_scm_socket =
            ScmSocket::new(stream_1.into_raw_fd()).expect("Could not create scm socket");
        let receiving_scm_socket =
            ScmSocket::new(stream_2.into_raw_fd()).expect("Could not create scm socket");

        let tcp_listener =
            std::net::TcpListener::bind("127.0.0.1:0").expect("Could not bind a TCP listener");
        let udp_socket =
            std::net::UdpSocket::bind("127.0.0.1:0").expect("Could not bind a UDP socket");
        let listeners = Listeners {
            http: vec![],
            tls: vec![],
            tcp: vec![(
                tcp_listener.local_addr().expect("TCP local address").into(),
                tcp_listener.as_raw_fd(),
            )],
            udp: vec![(
                udp_socket.local_addr().expect("UDP local address").into(),
                udp_socket.as_raw_fd(),
            )],
        };
        sending_scm_socket
            .send_listeners(&listeners)
            .expect("Could not send listeners");

        let received = receiving_scm_socket
            .receive_listeners()
            .expect("Could not receive listeners");
        let received_fds: Vec<RawFd> = received
            .tcp
            .iter()
            .chain(received.udp.iter())
            .map(|(_, fd)| *fd)
            .collect();
        assert_eq!(received_fds.len(), 2, "both descriptors must arrive");
        for fd in received_fds {
            // SAFETY: `fd` is a descriptor this test just received and still
            // owns; `F_GETFD` only reads its descriptor flags.
            let flags = unsafe { libc::fcntl(fd, libc::F_GETFD) };
            assert!(flags >= 0, "F_GETFD failed on received fd {fd}");
            assert_ne!(
                flags & libc::FD_CLOEXEC,
                0,
                "received fd {fd} must be close-on-exec so no exec'd child inherits it"
            );
        }
        received.close();
    }

    #[test]
    fn send_and_receive_empty_listeners() {
        let (stream_1, stream_2) =
            MioUnixStream::pair().expect("Could not create a pair of mio unix streams");

        let sending_scm_socket =
            ScmSocket::new(stream_1.into_raw_fd()).expect("Could not create scm socket");

        let receiving_scm_socket =
            ScmSocket::new(stream_2.as_raw_fd()).expect("Could not create scm socket");

        let listeners = Listeners::default();

        sending_scm_socket
            .send_listeners(&listeners)
            .expect("Could not send listeners");

        let received_listeners = receiving_scm_socket
            .receive_listeners()
            .expect("Could not receive listeners");

        assert_eq!(listeners, received_listeners);
    }

    /// The SCM hand-off (worker upgrade, main upgrade) tells listeners apart by
    /// interface: two sockets on one address, bound to different interfaces,
    /// cross the wire under their own keys and each is handed back only to the
    /// listener with that exact (address, interface).
    #[test]
    fn send_and_receive_listeners_keyed_by_interface() {
        let (stream_1, stream_2) =
            MioUnixStream::pair().expect("Could not create a pair of mio unix streams");
        let sending_scm_socket =
            ScmSocket::new(stream_1.into_raw_fd()).expect("Could not create scm socket");
        let receiving_scm_socket =
            ScmSocket::new(stream_2.into_raw_fd()).expect("Could not create scm socket");

        let (bare, wg0) =
            MioUnixStream::pair().expect("Could not create a pair of mio unix streams");
        let (lo, udp) = MioUnixStream::pair().expect("Could not create a pair of mio unix streams");

        let address = socket_addr_from_str("0.0.0.0:8443");
        let bare_key = ListenerKey::from(address);
        let wg0_key = ListenerKey::new(address, Some("wg0"));
        let lo_key = ListenerKey::new(address, Some("lo"));
        let listeners = Listeners {
            http: vec![
                (bare_key.clone(), bare.as_raw_fd()),
                (wg0_key.clone(), wg0.as_raw_fd()),
                (lo_key.clone(), lo.as_raw_fd()),
            ],
            udp: vec![(
                ListenerKey::new(socket_addr_from_str("[::]:5353"), Some("wg0")),
                udp.as_raw_fd(),
            )],
            ..Default::default()
        };

        sending_scm_socket
            .send_listeners(&listeners)
            .expect("Could not send listeners");
        let mut received = receiving_scm_socket
            .receive_listeners()
            .expect("Could not receive listeners");

        let keys: Vec<&ListenerKey> = received.http.iter().map(|(key, _)| key).collect();
        assert_eq!(keys, vec![&bare_key, &wg0_key, &lo_key]);
        assert_eq!(received.udp[0].0, listeners.udp[0].0);

        let wg0_fd = received.get_http(&wg0_key).expect("the wg0 socket");
        assert_eq!(received.http[1].0, lo_key, "only the wg0 entry is taken");
        assert!(received.get_http(&wg0_key).is_none());
        assert!(
            received
                .get_http(&ListenerKey::new(address, Some("eth0")))
                .is_none(),
            "an unknown interface on a known address matches nothing"
        );
        let bare_fd = received.get_http(&bare_key).expect("the bare socket");
        assert_ne!(wg0_fd, bare_fd);
    }

    #[test]
    fn send_and_receive_socket_addresses() {
        let (stream_1, stream_2) =
            MioUnixStream::pair().expect("Could not create a pair of mio unix streams");

        println!("unix stream pair: {stream_1:?} and {stream_2:?}");
        let sending_scm_socket =
            ScmSocket::new(stream_1.into_raw_fd()).expect("Could not create scm socket");

        println!("sending socket: {sending_scm_socket:?}");

        let receiving_scm_socket =
            ScmSocket::new(stream_2.into_raw_fd()).expect("Could not create scm socket");

        println!("receiving socket: {receiving_scm_socket:?}");

        // We have to provide actual file descriptors, even if they will all be changed in the takeover
        let (http_socket1, http_socket2) =
            MioUnixStream::pair().expect("Could not create a pair of mio unix streams");
        let (tcp_socket1, tcp_socket2) =
            MioUnixStream::pair().expect("Could not create a pair of mio unix streams");
        let (tls_socket1, tls_socket2) =
            MioUnixStream::pair().expect("Could not create a pair of mio unix streams");
        let (udp_socket1, udp_socket2) =
            MioUnixStream::pair().expect("Could not create a pair of mio unix streams");

        let listeners = Listeners {
            http: vec![
                (
                    socket_addr_from_str("127.0.1.1:8080").into(),
                    http_socket1.as_raw_fd(),
                ),
                (
                    socket_addr_from_str("127.0.1.2:8080").into(),
                    http_socket2.as_raw_fd(),
                ),
            ],
            tcp: vec![
                (
                    socket_addr_from_str("127.0.2.1:8080").into(),
                    tcp_socket1.as_raw_fd(),
                ),
                (
                    socket_addr_from_str("127.0.2.2:8080").into(),
                    tcp_socket2.as_raw_fd(),
                ),
            ],
            tls: vec![
                (
                    socket_addr_from_str("127.0.3.1:8443").into(),
                    tls_socket1.as_raw_fd(),
                ),
                (
                    socket_addr_from_str("127.0.3.2:8443").into(),
                    tls_socket2.as_raw_fd(),
                ),
            ],
            udp: vec![
                (
                    socket_addr_from_str("127.0.4.1:5353").into(),
                    udp_socket1.as_raw_fd(),
                ),
                (
                    socket_addr_from_str("127.0.4.2:5353").into(),
                    udp_socket2.as_raw_fd(),
                ),
            ],
        };

        println!("self.fd: {}", sending_scm_socket.fd);
        println!("listeners to send: {listeners:#?}");

        sending_scm_socket
            .send_listeners(&listeners)
            .expect("Could not send listeners");

        let received_listeners = receiving_scm_socket
            .receive_listeners()
            .expect("Could not receive listeners");

        assert_eq!(listeners.http[0].0, received_listeners.http[0].0);
        assert_eq!(listeners.udp.len(), received_listeners.udp.len());
        assert_eq!(listeners.udp[0].0, received_listeners.udp[0].0);
        assert_eq!(listeners.udp[1].0, received_listeners.udp[1].0);
    }

    /// Regression: a malformed `ListenersCount` whose entry counts do not
    /// match the number of file descriptors received over SCM must be
    /// rejected with `ListenersCountInconsistent`, never panic the worker
    /// on `received_fds[index..index + len]`.
    ///
    /// Without the bounds check, a peer that declares more addresses than
    /// `MAX_FDS_OUT` (or more than the FDs that actually arrived) crashes
    /// the receiving worker on out-of-bounds array indexing.
    #[test]
    fn rejects_listeners_count_with_more_entries_than_fds() {
        let (stream_1, stream_2) =
            MioUnixStream::pair().expect("Could not create a pair of mio unix streams");
        let sender = ScmSocket::new(stream_1.into_raw_fd()).expect("Could not create scm socket");
        let receiver = ScmSocket::new(stream_2.into_raw_fd()).expect("Could not create scm socket");

        // Declare three HTTP entries but ship zero file descriptors.
        let bogus = ListenersCount {
            http: vec![
                "127.0.0.1:80".to_string(),
                "127.0.0.2:80".to_string(),
                "127.0.0.3:80".to_string(),
            ],
            tls: vec![],
            tcp: vec![],
            udp: vec![],
        };
        let payload = bogus.encode_length_delimited_to_vec();
        sender
            .send_msg_and_fds(&payload, &[])
            .expect("manual send_msg_and_fds with zero fds must succeed at the syscall layer");

        match receiver.receive_listeners() {
            Err(ScmSocketError::ListenersCountInconsistent {
                http,
                tls,
                tcp,
                total,
                fds_received,
                max_fds,
            }) => {
                assert_eq!(http, 3);
                assert_eq!(tls, 0);
                assert_eq!(tcp, 0);
                assert_eq!(total, 3);
                assert_eq!(fds_received, 0);
                assert_eq!(max_fds, MAX_FDS_OUT);
            }
            other => panic!(
                "expected ListenersCountInconsistent, got {other:?}\n\
                 NOTE: a panic / OOM here means the SCM bounds check was reverted",
            ),
        }
    }

    /// A listener transfer is one logical receive on the Unix stream. Its
    /// sender must reject a descriptor table the matching receiver cannot hold.
    #[test]
    fn rejects_listener_batch_above_the_fd_frame_limit_before_sending() {
        let (stream_1, _stream_2) =
            MioUnixStream::pair().expect("Could not create a pair of mio unix streams");
        let sender = ScmSocket::new(stream_1.into_raw_fd()).expect("Could not create scm socket");
        let payload = UnixStream::pair().expect("Could not create payload unix streams");
        let address = socket_addr_from_str("127.0.0.1:8080");
        let listeners = Listeners {
            tcp: (0..=MAX_FDS_OUT)
                .map(|_| (address.into(), payload.0.as_raw_fd()))
                .collect(),
            ..Default::default()
        };

        assert!(
            sender.send_listeners(&listeners).is_err(),
            "a batch with more than MAX_FDS_OUT descriptors must be refused before sendmsg"
        );
    }

    /// The receiver's data buffer is part of the same frame contract as its
    /// ancillary descriptor table.
    #[test]
    fn rejects_listener_batch_above_the_message_frame_limit_before_sending() {
        let (stream_1, _stream_2) =
            MioUnixStream::pair().expect("Could not create a pair of mio unix streams");
        let sender = ScmSocket::new(stream_1.into_raw_fd()).expect("Could not create scm socket");
        let payload = UnixStream::pair().expect("Could not create payload unix streams");
        let interface = "x".repeat(MAX_BYTES_OUT);
        let listeners = Listeners {
            tcp: vec![(
                ListenerKey::new(socket_addr_from_str("127.0.0.1:8080"), Some(&interface)),
                payload.0.as_raw_fd(),
            )],
            ..Default::default()
        };

        assert!(
            sender.send_listeners(&listeners).is_err(),
            "a manifest larger than MAX_BYTES_OUT must be refused before sendmsg"
        );
    }

    /// Descriptors installed by `recvmsg` belong to the receiver even when a
    /// later manifest parse fails. The error path must close them.
    #[test]
    fn closes_received_descriptors_when_listener_manifest_parsing_fails() {
        let (stream_1, stream_2) =
            MioUnixStream::pair().expect("Could not create a pair of mio unix streams");
        let sender = ScmSocket::new(stream_1.into_raw_fd()).expect("Could not create scm socket");
        let receiver = ScmSocket::new(stream_2.into_raw_fd()).expect("Could not create scm socket");
        let (payload_reader, mut payload_writer) =
            UnixStream::pair().expect("Could not create payload unix streams");
        let manifest = ListenersCount {
            http: vec!["not-a-listener-key".to_owned()],
            tls: vec![],
            tcp: vec![],
            udp: vec![],
        }
        .encode_length_delimited_to_vec();

        sender
            .send_msg_and_fds(&manifest, &[payload_reader.as_raw_fd()])
            .expect("the kernel accepts the syntactically opaque manifest");
        drop(payload_reader);
        assert!(
            matches!(
                receiver.receive_listeners(),
                Err(ScmSocketError::WrongSocketAddress { .. })
            ),
            "the malformed listener key must be rejected"
        );
        assert_eq!(
            payload_writer.write(&[1]).expect_err(
                "the received duplicate must be closed, leaving the payload writer without a peer"
            ).kind(),
            std::io::ErrorKind::BrokenPipe
        );
    }

    /// Even when the control buffer truncates SCM_RIGHTS, every descriptor the
    /// kernel did deliver must be owned and closed before returning the error.
    #[test]
    fn closes_received_descriptors_when_control_data_is_truncated() {
        let (stream_1, stream_2) =
            MioUnixStream::pair().expect("Could not create a pair of mio unix streams");
        let sender = ScmSocket::new(stream_1.into_raw_fd()).expect("Could not create scm socket");
        let receiver = ScmSocket::new(stream_2.into_raw_fd()).expect("Could not create scm socket");
        let payloads = [
            UnixStream::pair().expect("Could not create payload unix streams"),
            UnixStream::pair().expect("Could not create payload unix streams"),
            UnixStream::pair().expect("Could not create payload unix streams"),
        ];
        let fds = payloads
            .iter()
            .map(|(reader, _)| reader.as_raw_fd())
            .collect::<Vec<_>>();

        sender
            .send_msg_and_fds(&[0], &fds)
            .expect("the kernel accepts the descriptor message");
        let mut writers = payloads
            .into_iter()
            .map(|(reader, writer)| {
                drop(reader);
                writer
            })
            .collect::<Vec<_>>();

        let mut message = [0; MAX_BYTES_OUT];
        let mut control = AlignedControlBuffer {
            _alignment: [],
            bytes: [0; CONTROL_MESSAGE_BUFFER_SIZE],
        };
        let one_fd_control_size = socket::cmsg_space::<RawFd>();
        assert!(matches!(
            receiver.receive_msg_and_fds_with_control(
                &mut message,
                &mut control.bytes[..one_fd_control_size],
                false,
            ),
            Err(ScmSocketError::ReceiveTruncated {
                data: false,
                control: true
            })
        ));

        for writer in &mut writers {
            assert_eq!(
                writer
                    .write(&[1])
                    .expect_err("all received duplicates must be closed")
                    .kind(),
                std::io::ErrorKind::BrokenPipe
            );
        }
    }
}
