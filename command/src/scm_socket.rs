//! SCM_RIGHTS socket for FD passing between master and workers.
//!
//! Wraps a `SeqPacket` unix socket and uses the `nix` SCM_RIGHTS helpers
//! to ship listener/accept FDs across the master ↔ worker boundary at
//! startup and across hot upgrades. The borrowed-FD wrappers
//! (`set_blocking`) hold an FD without taking ownership; the listener
//! teardown paths intentionally take ownership through
//! `TcpListener::from_raw_fd` so the FD is closed by drop.

use std::{
    io::{IoSlice, IoSliceMut},
    net::{AddrParseError, SocketAddr},
    os::unix::{
        io::{FromRawFd, IntoRawFd, RawFd},
        net::UnixStream as StdUnixStream,
    },
};

use mio::net::{TcpListener, UdpSocket};
use nix::{cmsg_space, sys::socket};
use prost::{DecodeError, Message};

use crate::proto::command::ListenersCount;

pub const MAX_FDS_OUT: usize = 200;
pub const MAX_BYTES_OUT: usize = 4096;

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
    #[error("Wrong socket address {address}: {error}")]
    WrongSocketAddress {
        address: String,
        error: AddrParseError,
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
            stream
                .set_nonblocking(!blocking)
                .map_err(|error| ScmSocketError::SetBlocking { blocking, error })?;
            let _dropped_fd = stream.into_raw_fd();
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
        let listeners_count = ListenersCount {
            http: listeners.http.iter().map(|t| t.0.to_string()).collect(),
            tls: listeners.tls.iter().map(|t| t.0.to_string()).collect(),
            tcp: listeners.tcp.iter().map(|t| t.0.to_string()).collect(),
            udp: listeners.udp.iter().map(|t| t.0.to_string()).collect(),
        };

        // The manifest is built 1:1 from the listener tables; each address slot
        // ships exactly one FD, so the per-protocol counts must agree on both
        // sides of the wire. The receiver reconstructs (address, fd) pairs by
        // zipping these counts against the FD array — drift here is the exact
        // bug class `command_channel_security_tests` guards.
        debug_assert_eq!(
            listeners_count.http.len(),
            listeners.http.len(),
            "http manifest count must match the http listener table"
        );
        debug_assert_eq!(
            listeners_count.tls.len(),
            listeners.tls.len(),
            "tls manifest count must match the tls listener table"
        );
        debug_assert_eq!(
            listeners_count.tcp.len(),
            listeners.tcp.len(),
            "tcp manifest count must match the tcp listener table"
        );

        let message = listeners_count.encode_length_delimited_to_vec();

        let mut file_descriptors: Vec<RawFd> = Vec::new();

        file_descriptors.extend(listeners.http.iter().map(|t| t.1));
        file_descriptors.extend(listeners.tls.iter().map(|t| t.1));
        file_descriptors.extend(listeners.tcp.iter().map(|t| t.1));
        file_descriptors.extend(listeners.udp.iter().map(|t| t.1));

        // The FD vector must reconcile with the address totals: one descriptor
        // per listener, folded http+tls+tcp+udp. If these disagree the receiver
        // would zip mismatched (address, fd) pairs.
        let address_total =
            listeners.http.len() + listeners.tls.len() + listeners.tcp.len() + listeners.udp.len();
        debug_assert_eq!(
            file_descriptors.len(),
            address_total,
            "the FD count sent must equal the total listener-address count (one FD per address)"
        );

        self.send_msg_and_fds(&message, &file_descriptors)
    }

    /// Receive and parse listeners (socket addresses and file descriptors) via an scm socket
    pub fn receive_listeners(&self) -> Result<Listeners, ScmSocketError> {
        let mut buf = vec![0; MAX_BYTES_OUT];

        let mut received_fds: [RawFd; MAX_FDS_OUT] = [0; MAX_FDS_OUT];

        let (size, file_descriptor_length) =
            self.receive_msg_and_fds(&mut buf, &mut received_fds)?;

        debug!("{} received :{:?}", self.fd, (size, file_descriptor_length));

        let listeners_count = ListenersCount::decode_length_delimited(&buf[..size])
            .map_err(ScmSocketError::DecodeError)?;

        // Validate the manifest before indexing into the fixed-size FD array.
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
        if total > MAX_FDS_OUT || total > file_descriptor_length {
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
        // fixed FD-array bound and the number of FDs that actually arrived.
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
            "total FD slots must fit the fixed-size received_fds array before indexing"
        );
        debug_assert!(
            total <= file_descriptor_length,
            "manifest total must not exceed the FDs actually received"
        );
        debug_assert!(
            total <= received_fds.len(),
            "every (address, fd) zip below must stay within the received_fds array"
        );

        let mut http_addresses = parse_addresses(&listeners_count.http)?;
        let mut tls_addresses = parse_addresses(&listeners_count.tls)?;
        let mut tcp_addresses = parse_addresses(&listeners_count.tcp)?;
        let mut udp_addresses = parse_addresses(&listeners_count.udp)?;

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
        // Each FD slice end must stay within the validated total (and thus the
        // array); pair-assert the slice window before every zip.
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
            socket::sendmsg::<()>(self.fd, &iov, &[], flags, None)
                .map_err(|error| ScmSocketError::Send(error.to_string()))?;
            return Ok(());
        };

        let control_message = [socket::ControlMessage::ScmRights(fds)];
        debug!("{} send with data", self.fd);
        socket::sendmsg::<()>(self.fd, &iov, &control_message, flags, None)
            .map_err(|error| ScmSocketError::Send(error.to_string()))?;
        Ok(())
    }

    /// Parse the message and receives file descriptors separately via the ControlMessage
    fn receive_msg_and_fds(
        &self,
        message: &mut [u8],
        fds: &mut [RawFd],
    ) -> Result<(usize, usize), ScmSocketError> {
        // Snapshot the buffer length before `message` is borrowed mutably by
        // `iov`; the received byte count is asserted against it below.
        let message_capacity = message.len();
        let mut cmsg = cmsg_space!([RawFd; MAX_FDS_OUT]);
        let mut iov = [IoSliceMut::new(message)];

        let flags = if self.blocking {
            socket::MsgFlags::empty()
        } else {
            socket::MsgFlags::MSG_DONTWAIT
        };

        let msg = socket::recvmsg::<()>(self.fd, &mut iov[..], Some(&mut cmsg), flags)
            .map_err(|error| ScmSocketError::Receive(error.to_string()))?;

        // The destination slice is the receiver's fixed `[RawFd; MAX_FDS_OUT]`
        // array; the zip below cannot write past it.
        let fds_capacity = fds.len();
        debug_assert!(
            fds_capacity <= MAX_FDS_OUT,
            "destination FD slice must not exceed the MAX_FDS_OUT cmsg space"
        );
        let mut fd_count = 0;
        let received_fds = msg
            .cmsgs()
            .map_err(|error| ScmSocketError::Receive(error.to_string()))?
            .filter_map(|cmsg| {
                if let socket::ControlMessageOwned::ScmRights(s) = cmsg {
                    Some(s)
                } else {
                    None
                }
            })
            .flatten();
        for (fd, place) in received_fds.zip(fds.iter_mut()) {
            fd_count += 1;
            *place = fd;
            // The zip is bounded by `fds.iter_mut()`, so each wrap stays within
            // the destination array — never write past `fds_capacity`.
            debug_assert!(
                fd_count <= fds_capacity,
                "received FD count must never exceed the destination array capacity"
            );
        }
        // Post-condition: the reported count reconciles with the destination
        // bound and the byte count is within the message buffer we handed in.
        debug_assert!(
            fd_count <= fds_capacity,
            "final received FD count must fit the destination array"
        );
        debug_assert!(
            msg.bytes <= message_capacity,
            "received byte count must not exceed the message buffer it was read into"
        );
        Ok((msg.bytes, fd_count))
    }
}

/// Socket addresses and file descriptors of listening sockets, needed by a
/// Proxy to start listening. The transport is fd-type-agnostic: `udp` carries
/// `UdpSocket` fds, the others carry `TcpListener` fds.
#[derive(Clone, Default, Debug, Serialize, Deserialize, PartialEq)]
pub struct Listeners {
    pub http: Vec<(SocketAddr, RawFd)>,
    pub tls: Vec<(SocketAddr, RawFd)>,
    pub tcp: Vec<(SocketAddr, RawFd)>,
    #[serde(default)]
    pub udp: Vec<(SocketAddr, RawFd)>,
}

impl Listeners {
    pub fn get_http(&mut self, addr: &SocketAddr) -> Option<RawFd> {
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

    pub fn get_https(&mut self, addr: &SocketAddr) -> Option<RawFd> {
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

    pub fn get_tcp(&mut self, addr: &SocketAddr) -> Option<RawFd> {
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

    pub fn get_udp(&mut self, addr: &SocketAddr) -> Option<RawFd> {
        self.udp
            .iter()
            .position(|(front, _)| front == addr)
            .map(|pos| self.udp.remove(pos).1)
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

fn parse_addresses(addresses: &[String]) -> Result<Vec<SocketAddr>, ScmSocketError> {
    let mut parsed_addresses = Vec::new();
    for address in addresses {
        parsed_addresses.push(address.parse::<SocketAddr>().map_err(|error| {
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

    use std::{net::SocketAddr, os::unix::prelude::AsRawFd, str::FromStr};

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
                    socket_addr_from_str("127.0.1.1:8080"),
                    http_socket1.as_raw_fd(),
                ),
                (
                    socket_addr_from_str("127.0.1.2:8080"),
                    http_socket2.as_raw_fd(),
                ),
            ],
            tcp: vec![
                (
                    socket_addr_from_str("127.0.2.1:8080"),
                    tcp_socket1.as_raw_fd(),
                ),
                (
                    socket_addr_from_str("127.0.2.2:8080"),
                    tcp_socket2.as_raw_fd(),
                ),
            ],
            tls: vec![
                (
                    socket_addr_from_str("127.0.3.1:8443"),
                    tls_socket1.as_raw_fd(),
                ),
                (
                    socket_addr_from_str("127.0.3.2:8443"),
                    tls_socket2.as_raw_fd(),
                ),
            ],
            udp: vec![
                (
                    socket_addr_from_str("127.0.4.1:5353"),
                    udp_socket1.as_raw_fd(),
                ),
                (
                    socket_addr_from_str("127.0.4.2:5353"),
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
}
