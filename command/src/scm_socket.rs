use std::{
    io::{IoSlice, IoSliceMut},
    net::{AddrParseError, SocketAddr},
    os::unix::{
        io::{FromRawFd, IntoRawFd, RawFd},
        net::UnixStream as StdUnixStream,
    },
};

use mio::net::TcpListener;
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
        unsafe {
            let stream = StdUnixStream::from_raw_fd(self.fd);
            stream
                .set_nonblocking(!blocking)
                .map_err(|error| ScmSocketError::SetBlocking { blocking, error })?;
            let _dropped_fd = stream.into_raw_fd();
        }
        self.blocking = blocking;
        Ok(())
    }

    /// Send listeners (socket addresses and file descriptors) via an scm socket
    pub fn send_listeners(&self, listeners: &Listeners) -> Result<(), ScmSocketError> {
        let listeners_count = ListenersCount {
            http: listeners.http.iter().map(|t| t.0.to_string()).collect(),
            tls: listeners.tls.iter().map(|t| t.0.to_string()).collect(),
            tcp: listeners.tcp.iter().map(|t| t.0.to_string()).collect(),
        };

        let message = listeners_count.encode_length_delimited_to_vec();

        let mut file_descriptors: Vec<RawFd> = Vec::new();

        file_descriptors.extend(listeners.http.iter().map(|t| t.1));
        file_descriptors.extend(listeners.tls.iter().map(|t| t.1));
        file_descriptors.extend(listeners.tcp.iter().map(|t| t.1));

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

        let mut http_addresses = parse_addresses(&listeners_count.http)?;
        let mut tls_addresses = parse_addresses(&listeners_count.tls)?;
        let mut tcp_addresses = parse_addresses(&listeners_count.tcp)?;

        let mut index = 0;
        let len = listeners_count.http.len();
        let mut http = Vec::new();
        http.extend(
            http_addresses
                .drain(..)
                .zip(received_fds[index..index + len].iter().cloned()),
        );

        index += len;
        let len = listeners_count.tls.len();
        let mut tls = Vec::new();
        tls.extend(
            tls_addresses
                .drain(..)
                .zip(received_fds[index..index + len].iter().cloned()),
        );

        index += len;
        let mut tcp = Vec::new();
        tcp.extend(
            tcp_addresses
                .drain(..)
                .zip(received_fds[index..file_descriptor_length].iter().cloned()),
        );

        Ok(Listeners { http, tls, tcp })
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
        let mut cmsg = cmsg_space!([RawFd; MAX_FDS_OUT]);
        let mut iov = [IoSliceMut::new(message)];

        let flags = if self.blocking {
            socket::MsgFlags::empty()
        } else {
            socket::MsgFlags::MSG_DONTWAIT
        };

        let msg = socket::recvmsg::<()>(self.fd, &mut iov[..], Some(&mut cmsg), flags)
            .map_err(|error| ScmSocketError::Receive(error.to_string()))?;

        let mut fd_count = 0;
        let received_fds = msg
            .cmsgs()
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
        }
        Ok((msg.bytes, fd_count))
    }
}

/// Socket addresses and file descriptors of TCP sockets, needed by a Proxy to start listening
#[derive(Clone, Default, Debug, Serialize, Deserialize, PartialEq)]
pub struct Listeners {
    pub http: Vec<(SocketAddr, RawFd)>,
    pub tls: Vec<(SocketAddr, RawFd)>,
    pub tcp: Vec<(SocketAddr, RawFd)>,
}

impl Listeners {
    pub fn get_http(&mut self, addr: &SocketAddr) -> Option<RawFd> {
        self.http
            .iter()
            .position(|(front, _)| front == addr)
            .map(|pos| self.http.remove(pos).1)
    }

    pub fn get_https(&mut self, addr: &SocketAddr) -> Option<RawFd> {
        self.tls
            .iter()
            .position(|(front, _)| front == addr)
            .map(|pos| self.tls.remove(pos).1)
    }

    pub fn get_tcp(&mut self, addr: &SocketAddr) -> Option<RawFd> {
        self.tcp
            .iter()
            .position(|(front, _)| front == addr)
            .map(|pos| self.tcp.remove(pos).1)
    }

    /// Deactivate all listeners by closing their file descriptors
    pub fn close(&self) {
        for (_, ref fd) in &self.http {
            unsafe {
                let _ = TcpListener::from_raw_fd(*fd);
            }
        }

        for (_, ref fd) in &self.tls {
            unsafe {
                let _ = TcpListener::from_raw_fd(*fd);
            }
        }

        for (_, ref fd) in &self.tcp {
            unsafe {
                let _ = TcpListener::from_raw_fd(*fd);
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

    use super::*;
    use mio::net::UnixStream as MioUnixStream;
    use std::{net::SocketAddr, os::unix::prelude::AsRawFd, str::FromStr};

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
    }
}
