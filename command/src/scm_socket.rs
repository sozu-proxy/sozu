use std::{
    io::{IoSlice, IoSliceMut},
    net::SocketAddr,
    os::unix::{
        io::{FromRawFd, IntoRawFd, RawFd},
        net::UnixStream as StdUnixStream,
    },
    str::from_utf8,
};

use anyhow::Context;
use mio::net::TcpListener;
use nix::{cmsg_space, sys::socket};
use serde_json;

pub const MAX_FDS_OUT: usize = 200;
pub const MAX_BYTES_OUT: usize = 4096;

/// A unix socket specialized for file descriptor passing
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct ScmSocket {
    pub fd: RawFd,
    pub blocking: bool,
}

impl ScmSocket {
    /// Create a blocking SCM socket from a raw file descriptor (unsafe)
    pub fn new(fd: RawFd) -> anyhow::Result<Self> {
        unsafe {
            let stream = StdUnixStream::from_raw_fd(fd);
            stream
                .set_nonblocking(false)
                .with_context(|| "could not change blocking status for stream")?;
            let _dropped_fd = stream.into_raw_fd();
        }

        Ok(ScmSocket { fd, blocking: true })
    }

    /// Get the raw file descriptor of the scm channel
    pub fn raw_fd(&self) -> i32 {
        self.fd
    }

    /// Use the standard library (unsafe) to set the socket to blocking / unblocking
    pub fn set_blocking(&mut self, blocking: bool) -> anyhow::Result<()> {
        if self.blocking == blocking {
            return Ok(());
        }
        unsafe {
            let stream = StdUnixStream::from_raw_fd(self.fd);
            stream
                .set_nonblocking(!blocking)
                .with_context(|| "could not change blocking status for stream")?;
            let _dropped_fd = stream.into_raw_fd();
        }
        self.blocking = blocking;
        Ok(())
    }

    /// Send listeners (socket addresses and file descriptors) via an scm socket
    pub fn send_listeners(&self, listeners: &Listeners) -> anyhow::Result<()> {
        let listeners_count = ListenersCount {
            http: listeners.http.iter().map(|t| t.0).collect(),
            tls: listeners.tls.iter().map(|t| t.0).collect(),
            tcp: listeners.tcp.iter().map(|t| t.0).collect(),
        };

        let message = serde_json::to_string(&listeners_count)
            .map(|s| s.into_bytes())
            .unwrap_or_else(|_| Vec::new());

        let mut file_descriptors: Vec<RawFd> = Vec::new();

        file_descriptors.extend(listeners.http.iter().map(|t| t.1));
        file_descriptors.extend(listeners.tls.iter().map(|t| t.1));
        file_descriptors.extend(listeners.tcp.iter().map(|t| t.1));

        self.send_msg_and_fds(&message, &file_descriptors)
    }

    /// Receive and parse listeners (socket addresses and file descriptors) via an scm socket
    pub fn receive_listeners(&self) -> anyhow::Result<Listeners> {
        let mut buf = vec![0; MAX_BYTES_OUT];

        let mut received_fds: [RawFd; MAX_FDS_OUT] = [0; MAX_FDS_OUT];

        let (size, file_descriptor_length) = self
            .receive_msg_and_fds(&mut buf, &mut received_fds)
            .with_context(|| "could not receive listeners")?;

        debug!("{} received :{:?}", self.fd, (size, file_descriptor_length));

        let raw_listener_list =
            from_utf8(&buf[..size]).with_context(|| "Could not parse utf8 string from buffer")?;

        let mut listeners_count = serde_json::from_str::<ListenersCount>(raw_listener_list)
            .with_context(|| "Could not deserialize utf8 string into listeners")?;

        let mut index = 0;
        let len = listeners_count.http.len();
        let mut http = Vec::new();
        http.extend(
            listeners_count
                .http
                .drain(..)
                .zip(received_fds[index..index + len].iter().cloned()),
        );

        index += len;
        let len = listeners_count.tls.len();
        let mut tls = Vec::new();
        tls.extend(
            listeners_count
                .tls
                .drain(..)
                .zip(received_fds[index..index + len].iter().cloned()),
        );

        index += len;
        let mut tcp = Vec::new();
        tcp.extend(
            listeners_count
                .tcp
                .drain(..)
                .zip(received_fds[index..file_descriptor_length].iter().cloned()),
        );

        Ok(Listeners { http, tls, tcp })
    }

    /// Sends message and file descriptors separately. The file descriptors are summed up
    /// in a ControlMessage.
    fn send_msg_and_fds(&self, message: &[u8], fds: &[RawFd]) -> anyhow::Result<()> {
        let iov = [IoSlice::new(message)];
        let flags = if self.blocking {
            socket::MsgFlags::empty()
        } else {
            socket::MsgFlags::MSG_DONTWAIT
        };

        if fds.is_empty() {
            println!("{} send empty", self.fd);
            socket::sendmsg::<()>(self.fd, &iov, &[], flags, None)
                .with_context(|| "Could not send empty message per socket")?;
            return Ok(());
        };

        let control_message = [socket::ControlMessage::ScmRights(fds)];
        println!("{} send with data", self.fd);
        socket::sendmsg::<()>(self.fd, &iov, &control_message, flags, None)
            .with_context(|| "Could not send message per socket")?;
        Ok(())
    }

    /// Parse the message and receives file descriptors separately via the ControlMessage
    fn receive_msg_and_fds(
        &self,
        message: &mut [u8],
        fds: &mut [RawFd],
    ) -> anyhow::Result<(usize, usize)> {
        let mut cmsg = cmsg_space!([RawFd; MAX_FDS_OUT]);
        let mut iov = [IoSliceMut::new(message)];

        let flags = if self.blocking {
            socket::MsgFlags::empty()
        } else {
            socket::MsgFlags::MSG_DONTWAIT
        };

        let msg = socket::recvmsg::<()>(self.fd, &mut iov[..], Some(&mut cmsg), flags)
            .with_context(|| "Could not receive message per socket")?;

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

/// Socket addresses and file descriptors needed by a Proxy to start listening
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq)]
pub struct Listeners {
    pub http: Vec<(SocketAddr, RawFd)>,
    pub tls: Vec<(SocketAddr, RawFd)>,
    pub tcp: Vec<(SocketAddr, RawFd)>,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
struct ListenersCount {
    pub http: Vec<SocketAddr>,
    pub tls: Vec<SocketAddr>,
    pub tcp: Vec<SocketAddr>,
}

impl Listeners {
    pub fn get_http(&mut self, addr: &SocketAddr) -> Option<RawFd> {
        self.http
            .iter()
            .position(|(front, _)| front == addr)
            .and_then(|pos| Some(self.http.remove(pos).1))
    }

    pub fn get_https(&mut self, addr: &SocketAddr) -> Option<RawFd> {
        self.tls
            .iter()
            .position(|(front, _)| front == addr)
            .and_then(|pos| Some(self.tls.remove(pos).1))
    }

    pub fn get_tcp(&mut self, addr: &SocketAddr) -> Option<RawFd> {
        self.tcp
            .iter()
            .position(|(front, _)| front == addr)
            .and_then(|pos| Some(self.tcp.remove(pos).1))
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
        SocketAddr::from_str(str).expect(&format!(
            "failed to create socket address from string slice {}",
            str
        ))
    }

    #[test]
    fn send_and_receive_empty_listeners() {
        let (stream_1, stream_2) =
            MioUnixStream::pair().expect("Could not create a pair of mio unix streams");

        let sending_scm_socket =
            ScmSocket::new(stream_1.into_raw_fd()).expect("Could not create scm socket");

        let receiving_scm_socket =
            ScmSocket::new(stream_2.as_raw_fd()).expect("Could not create scm socket");

        let listeners = Listeners {
            http: vec![],
            tcp: vec![],
            tls: vec![],
        };

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

        println!("unix stream pair: {:?} and {:?}", stream_1, stream_2);
        let sending_scm_socket =
            ScmSocket::new(stream_1.into_raw_fd()).expect("Could not create scm socket");

        println!("sending socket: {:?}", sending_scm_socket);

        let receiving_scm_socket =
            ScmSocket::new(stream_2.into_raw_fd()).expect("Could not create scm socket");

        println!("receiving socket: {:?}", receiving_scm_socket);

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
        println!("listeners to send: {:#?}", listeners);

        sending_scm_socket
            .send_listeners(&listeners)
            .expect("Could not send listeners");

        let received_listeners = receiving_scm_socket
            .receive_listeners()
            .expect("Could not receive listeners");

        assert_eq!(listeners.http[0].0, received_listeners.http[0].0);
    }
}
