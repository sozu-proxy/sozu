//! Mock UDP backend for the end-to-end UDP load-balancing tests (issue #1273).
//!
//! UDP has no accept loop: a backend is a single `UdpSocket` that receives
//! datagrams and replies to whichever source they arrived from (here, the
//! proxy's per-flow connected upstream socket — the symmetric-NAT return key).
//! The backend runs on its own thread because `recv_from` blocks; the test
//! drives the client directly and inspects the backend through the shared
//! [`UdpBackendHandle`].
//!
//! Capabilities the tests rely on:
//! - configurable number of replies per received datagram (`replies_per_request`,
//!   default `1`) so we can model DNS-style one-shot backends and silent sinks
//!   (`0`);
//! - a per-datagram byte tag (`{name}:{payload}`) so a test can tell which
//!   backend answered without depending on the proxy's LB policy;
//! - PROXY-protocol-v2 DGRAM header parsing: when the proxy prepends a PPv2
//!   header (cluster `send_proxy_protocol = true`), the backend strips it,
//!   records the advertised real client address, and replies with the *inner*
//!   payload so NAT-return assertions stay payload-exact;
//! - a record of every observed peer source address (the proxy's upstream
//!   socket address) and request count, exposed for distribution assertions.

use std::{
    net::{IpAddr, Ipv4Addr, Ipv6Addr, SocketAddr, UdpSocket},
    sync::{
        Arc, Mutex,
        atomic::{AtomicBool, Ordering},
    },
    thread::{self, JoinHandle},
    time::Duration,
};

/// The PROXY-protocol-v2 12-byte signature (shared with the TCP serializer).
const PP2_SIGNATURE: [u8; 12] = [
    0x0D, 0x0A, 0x0D, 0x0A, 0x00, 0x0D, 0x0A, 0x51, 0x55, 0x49, 0x54, 0x0A,
];

/// What the backend observed for a single received datagram.
#[derive(Clone, Debug)]
pub struct ObservedDatagram {
    /// The transport source the datagram arrived from (the proxy's per-flow
    /// connected upstream socket address — the NAT-return demux key).
    pub peer: SocketAddr,
    /// The real (pre-NAT) client address advertised in a PPv2 DGRAM header,
    /// when one was present; `None` for an un-proxied datagram.
    pub proxy_protocol_client: Option<SocketAddr>,
    /// The inner application payload (PPv2 header stripped when present).
    pub payload: Vec<u8>,
}

/// Shared, lock-guarded view of a running [`UdpBackend`].
#[derive(Default)]
struct Shared {
    observed: Vec<ObservedDatagram>,
}

/// A handle to a backend running on a detached thread. Cloneable; the test
/// keeps one and the thread keeps one.
#[derive(Clone)]
pub struct UdpBackendHandle {
    pub name: String,
    pub address: SocketAddr,
    shared: Arc<Mutex<Shared>>,
    running: Arc<AtomicBool>,
    join: Arc<Mutex<Option<JoinHandle<()>>>>,
}

impl UdpBackendHandle {
    /// Total datagrams received so far.
    pub fn requests_received(&self) -> usize {
        self.shared
            .lock()
            .expect("backend lock poisoned")
            .observed
            .len()
    }

    /// A snapshot of every datagram observed so far.
    pub fn observed(&self) -> Vec<ObservedDatagram> {
        self.shared
            .lock()
            .expect("backend lock poisoned")
            .observed
            .clone()
    }

    /// The inner payloads observed so far, decoded as UTF-8 (lossy).
    pub fn payloads_utf8(&self) -> Vec<String> {
        self.observed()
            .into_iter()
            .map(|d| String::from_utf8_lossy(&d.payload).into_owned())
            .collect()
    }

    /// Block until at least `count` datagrams have arrived, or the deadline
    /// elapses. Returns whether the count was reached.
    pub fn wait_for_requests(&self, count: usize, timeout: Duration) -> bool {
        let deadline = std::time::Instant::now() + timeout;
        loop {
            if self.requests_received() >= count {
                return true;
            }
            if std::time::Instant::now() >= deadline {
                return false;
            }
            thread::sleep(Duration::from_millis(5));
        }
    }

    /// Stop the backend thread and join it. Idempotent.
    pub fn stop(&self) {
        self.running.store(false, Ordering::SeqCst);
        // Nudge the blocking recv loop awake with a self-addressed datagram.
        if let Ok(sock) = UdpSocket::bind((Ipv4Addr::LOCALHOST, 0)) {
            let _ = sock.send_to(b"__stop__", self.address);
        }
        if let Some(handle) = self.join.lock().expect("join lock poisoned").take() {
            let _ = handle.join();
        }
    }
}

/// A mock UDP backend. Construct, then [`spawn`](UdpBackend::spawn) it onto a
/// detached thread that owns the `UdpSocket`.
pub struct UdpBackend {
    name: String,
    address: SocketAddr,
    socket: UdpSocket,
    replies_per_request: usize,
}

impl UdpBackend {
    /// Bind a backend socket on `address`. `replies_per_request` controls how
    /// many reply datagrams are sent back per received datagram (`0` = a silent
    /// sink, `1` = DNS-style one-shot, `n` = a chatty backend).
    pub fn bind<S: Into<String>>(name: S, address: SocketAddr, replies_per_request: usize) -> Self {
        let socket = UdpSocket::bind(address)
            .unwrap_or_else(|e| panic!("udp backend: could not bind {address}: {e}"));
        socket
            .set_read_timeout(Some(Duration::from_millis(100)))
            .expect("udp backend: set read timeout");
        Self {
            name: name.into(),
            address,
            socket,
            replies_per_request,
        }
    }

    /// Spawn the backend onto a detached thread and return a handle. The thread
    /// echoes back a `{name}:{payload}`-tagged reply for every received
    /// datagram (PPv2 header stripped, real client recorded).
    pub fn spawn(self) -> UdpBackendHandle {
        let shared = Arc::new(Mutex::new(Shared::default()));
        let running = Arc::new(AtomicBool::new(true));
        let name = self.name.clone();
        let address = self.address;
        let socket = self.socket;
        let replies = self.replies_per_request;
        let thread_shared = shared.clone();
        let thread_running = running.clone();
        let thread_name = name.clone();

        let join = thread::Builder::new()
            .name(format!("udp-backend-{name}"))
            .spawn(move || {
                let mut buf = vec![0u8; 65_535];
                while thread_running.load(Ordering::SeqCst) {
                    let (len, peer) = match socket.recv_from(&mut buf) {
                        Ok(v) => v,
                        Err(ref e)
                            if e.kind() == std::io::ErrorKind::WouldBlock
                                || e.kind() == std::io::ErrorKind::TimedOut =>
                        {
                            continue;
                        }
                        Err(_) => continue,
                    };
                    let raw = &buf[..len];
                    if raw == b"__stop__" {
                        break;
                    }
                    let (proxy_protocol_client, payload) = parse_proxy_protocol(raw);
                    let payload = payload.to_vec();
                    thread_shared
                        .lock()
                        .expect("backend lock poisoned")
                        .observed
                        .push(ObservedDatagram {
                            peer,
                            proxy_protocol_client,
                            payload: payload.clone(),
                        });
                    // Reply N times to the source we received from. UDP replies
                    // travel back over the proxy's connected upstream socket
                    // (symmetric NAT return).
                    let mut reply = thread_name.clone().into_bytes();
                    reply.push(b':');
                    reply.extend_from_slice(&payload);
                    for _ in 0..replies {
                        let _ = socket.send_to(&reply, peer);
                    }
                }
            })
            .expect("could not spawn udp backend thread");

        let _ = address;
        UdpBackendHandle {
            name,
            address,
            shared,
            running,
            join: Arc::new(Mutex::new(Some(join))),
        }
    }
}

/// Parse an optional PPv2 DGRAM header off the front of a received datagram.
/// Returns `(advertised_client, inner_payload)`. When no valid PPv2 header is
/// present, returns `(None, whole_datagram)`.
fn parse_proxy_protocol(raw: &[u8]) -> (Option<SocketAddr>, &[u8]) {
    if raw.len() < 16 || raw[..12] != PP2_SIGNATURE {
        return (None, raw);
    }
    // Byte 12: version+command (0x21 = v2 + PROXY). Byte 13: family+transport.
    let ver_cmd = raw[12];
    if ver_cmd >> 4 != 0x2 {
        return (None, raw);
    }
    let fam = raw[13];
    let addr_len = u16::from_be_bytes([raw[14], raw[15]]) as usize;
    let body_start = 16;
    if raw.len() < body_start + addr_len {
        return (None, raw);
    }
    let body = &raw[body_start..body_start + addr_len];
    let inner = &raw[body_start + addr_len..];
    let client = match fam {
        // AF_INET | DGRAM: 4 + 4 + 2 + 2 = 12 bytes.
        0x12 if addr_len >= 12 => {
            let src_ip = Ipv4Addr::new(body[0], body[1], body[2], body[3]);
            let src_port = u16::from_be_bytes([body[8], body[9]]);
            Some(SocketAddr::new(IpAddr::V4(src_ip), src_port))
        }
        // AF_INET6 | DGRAM: 16 + 16 + 2 + 2 = 36 bytes.
        0x22 if addr_len >= 36 => {
            let mut octets = [0u8; 16];
            octets.copy_from_slice(&body[0..16]);
            let src_ip = Ipv6Addr::from(octets);
            let src_port = u16::from_be_bytes([body[32], body[33]]);
            Some(SocketAddr::new(IpAddr::V6(src_ip), src_port))
        }
        // AF_UNSPEC / unknown: treat as un-proxied.
        _ => None,
    };
    (client, inner)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_ppv2_ipv4_header() {
        // signature + 0x21 + 0x12 + len(12) + src 1.2.3.4 + dst 5.6.7.8 +
        // src port 1234 + dst port 5678, then payload "PING".
        let mut dg = PP2_SIGNATURE.to_vec();
        dg.push(0x21);
        dg.push(0x12);
        dg.extend_from_slice(&12u16.to_be_bytes());
        dg.extend_from_slice(&[1, 2, 3, 4]);
        dg.extend_from_slice(&[5, 6, 7, 8]);
        dg.extend_from_slice(&1234u16.to_be_bytes());
        dg.extend_from_slice(&5678u16.to_be_bytes());
        dg.extend_from_slice(b"PING");
        let (client, payload) = parse_proxy_protocol(&dg);
        assert_eq!(
            client,
            Some(SocketAddr::new(IpAddr::V4(Ipv4Addr::new(1, 2, 3, 4)), 1234))
        );
        assert_eq!(payload, b"PING");
    }

    #[test]
    fn un_proxied_datagram_passes_through() {
        let (client, payload) = parse_proxy_protocol(b"hello");
        assert_eq!(client, None);
        assert_eq!(payload, b"hello");
    }
}
