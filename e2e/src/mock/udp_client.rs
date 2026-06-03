//! Mock UDP client for the end-to-end UDP load-balancing tests (issue #1273).
//!
//! A thin wrapper over a `std::net::UdpSocket` connected to the proxy front
//! address. UDP is lossy and unordered, so every recv is deadline-bounded and
//! the helpers loop rather than assume a single datagram is the whole answer
//! (the UDP analogue of the `loop_read_*` discipline the TCP/H2 tests use).
//!
//! Each client binds an ephemeral source port; that source address is the flow
//! key the proxy keys affinity on, so a test that wants two distinct flows uses
//! two clients (distinct source ports), and a test that wants flow affinity
//! reuses the same client.

use std::{
    net::{Ipv4Addr, SocketAddr, UdpSocket},
    time::{Duration, Instant},
};

/// A UDP client bound to an ephemeral local port, talking to one proxy front.
pub struct UdpClient {
    pub name: String,
    pub socket: UdpSocket,
    pub front: SocketAddr,
}

impl UdpClient {
    /// Bind an ephemeral local socket and target `front`.
    pub fn new<S: Into<String>>(name: S, front: SocketAddr) -> Self {
        let socket = UdpSocket::bind((Ipv4Addr::LOCALHOST, 0))
            .expect("udp client: could not bind ephemeral socket");
        socket
            .set_read_timeout(Some(Duration::from_millis(100)))
            .expect("udp client: set read timeout");
        Self {
            name: name.into(),
            socket,
            front,
        }
    }

    /// The client's own (bound) source address — the flow key the proxy sees.
    pub fn local_addr(&self) -> SocketAddr {
        self.socket.local_addr().expect("udp client: local_addr")
    }

    /// Send one datagram to the proxy front.
    pub fn send(&self, payload: &[u8]) {
        self.socket
            .send_to(payload, self.front)
            .unwrap_or_else(|e| panic!("udp client {}: send failed: {e}", self.name));
    }

    /// Receive one datagram, bounded by `timeout`. Returns `None` on timeout.
    pub fn recv(&self, timeout: Duration) -> Option<Vec<u8>> {
        let deadline = Instant::now() + timeout;
        let mut buf = vec![0u8; 65_535];
        loop {
            let remaining = deadline.saturating_duration_since(Instant::now());
            if remaining.is_zero() {
                return None;
            }
            // Bound the per-call blocking to the remaining budget so a quiet
            // socket cannot overshoot the deadline by a full read timeout.
            let _ = self
                .socket
                .set_read_timeout(Some(remaining.min(Duration::from_millis(100))));
            match self.socket.recv(&mut buf) {
                Ok(len) => return Some(buf[..len].to_vec()),
                Err(ref e)
                    if e.kind() == std::io::ErrorKind::WouldBlock
                        || e.kind() == std::io::ErrorKind::TimedOut =>
                {
                    continue;
                }
                Err(_) => return None,
            }
        }
    }

    /// Receive one datagram and decode it as a UTF-8 string (lossy).
    pub fn recv_string(&self, timeout: Duration) -> Option<String> {
        self.recv(timeout)
            .map(|b| String::from_utf8_lossy(&b).into_owned())
    }

    /// Send a datagram then wait for one reply within `timeout`.
    pub fn round_trip(&self, payload: &[u8], timeout: Duration) -> Option<Vec<u8>> {
        self.send(payload);
        self.recv(timeout)
    }
}
