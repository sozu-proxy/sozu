//! Socket I/O wrappers and TCP option helpers.
//!
//! Hosts the `SocketHandler` trait, the `FrontRustls` wrapper that drives
//! a rustls `ServerConnection` over a `TcpStream`, plus the ancillary
//! `getsockopt(TCP_INFO)` / TCP-keepalive helpers. The
//! `FrontRustls::socket_write` / `socket_write_vectored` pair is a known
//! truncation hot spot — keep the two paths structurally symmetric (see
//! the per-method `///` invariants).

use std::{
    io::{ErrorKind, Read, Write},
    net::SocketAddr,
};

use mio::net::{TcpListener, TcpStream, UdpSocket};
use rustls::{ProtocolVersion, ServerConnection};
use rusty_ulid::Ulid;
use socket2::{Domain, Protocol, Socket, Type};
use sozu_command::{config::MAX_LOOP_ITERATIONS, logging::ansi_palette, ready::Ready};

use crate::metrics::names;

#[derive(thiserror::Error, Debug)]
pub enum ServerBindError {
    #[error("could not set bind to socket: {0}")]
    BindError(std::io::Error),
    #[error("could not listen on socket: {0}")]
    Listen(std::io::Error),
    #[error("could not set socket to nonblocking: {0}")]
    SetNonBlocking(std::io::Error),
    #[error("could not set reuse address: {0}")]
    SetReuseAddress(std::io::Error),
    #[error("could not set reuse address: {0}")]
    SetReusePort(std::io::Error),
    #[error("Could not create socket: {0}")]
    SocketCreationError(std::io::Error),
    #[error("Invalid socket address '{address}': {error}")]
    InvalidSocketAddress { address: String, error: String },
    #[error(
        "could not bind the listener {address} to the network interface '{interface}' \
         (SO_BINDTODEVICE): {error}. The interface must exist when the listener is activated, \
         and on Linux kernels before 5.7 binding a socket to an interface requires the \
         CAP_NET_RAW capability"
    )]
    BindToDevice {
        address: SocketAddr,
        interface: String,
        error: std::io::Error,
    },
    #[error(
        "the listener {address} names the network interface '{interface}', but binding a \
         socket to an interface (SO_BINDTODEVICE) is only supported on Linux"
    )]
    BindToDeviceUnsupported {
        address: SocketAddr,
        interface: String,
    },
}

#[derive(Debug, PartialEq, Eq, Copy, Clone)]
pub enum SocketResult {
    Continue,
    Closed,
    WouldBlock,
    Error,
}

#[derive(Debug, PartialEq, Eq, Copy, Clone)]
pub enum TransportProtocol {
    Tcp,
    Ssl2,
    Ssl3,
    Tls1_0,
    Tls1_1,
    Tls1_2,
    Tls1_3,
}

pub trait SocketHandler {
    fn socket_read(&mut self, buf: &mut [u8]) -> (usize, SocketResult);
    fn socket_write(&mut self, buf: &[u8]) -> (usize, SocketResult);
    fn socket_write_vectored(&mut self, _buf: &[std::io::IoSlice]) -> (usize, SocketResult);
    fn socket_wants_write(&self) -> bool {
        false
    }
    fn socket_close(&mut self) {}
    /// Write `buf`, then begin the close [`Self::socket_close`] begins, and
    /// report what [`Self::socket_write`] would for `buf`.
    ///
    /// The close is begun only when the whole of `buf` was accepted, so a
    /// caller that gets back less resumes with the rest and the close still
    /// follows its last byte. A handler without a close of its own does
    /// exactly the two calls; [`FrontRustls`] puts the plaintext and its
    /// `close_notify` behind one flush, so the last application bytes and
    /// the alert leave in one `writev(2)` instead of two.
    fn socket_write_then_close(&mut self, buf: &[u8]) -> (usize, SocketResult) {
        let (size, status) = self.socket_write(buf);
        if size == buf.len() {
            self.socket_close();
        }
        (size, status)
    }
    fn socket_ref(&self) -> &TcpStream;
    fn socket_mut(&mut self) -> &mut TcpStream;
    /// Peer address of this connection, preferring a snapshot taken when the
    /// handler was built over a live `getpeername(2)`.
    ///
    /// `getpeername(2)` fails with `ENOTCONN` the moment the peer resets, so a
    /// live lookup renders `peer=None` on exactly the error lines an operator
    /// reads during an incident. It also reports the transport peer, which on a
    /// PROXY-protocol frontend is the load balancer rather than the client the
    /// rest of the session is logged against.
    ///
    /// Implementations that cache an address ([`SessionTcpStream`],
    /// [`FrontRustls`]) return it and fall back to the live lookup only when
    /// they have none; the raw [`TcpStream`] impl carries no context and has
    /// nothing but the live lookup. Deliberately has no default body: a new
    /// handler must state which of the two it is.
    fn peer_addr(&self) -> Option<SocketAddr>;
    fn protocol(&self) -> TransportProtocol;
    fn read_error(&self);
    fn write_error(&self);
    /// Returns the owning connection's session ULID when known. Used by
    /// `log_socket_context!` to render the `[<session_ulid> - - -]` segment
    /// of the socket-layer log prefix, matching the format used by the
    /// rest of the mux stack. Returns `None` for contextless implementations
    /// (e.g. raw `mio::TcpStream`); the macro renders `-` in the ULID slot.
    fn session_ulid(&self) -> Option<Ulid> {
        None
    }
}

/// Format the socket-layer log prefix `[<session_ulid_or_->]\tSOCKET\tSession(
/// peer=..., local=..., rtt=..., state=..., protocol=...)\t >>>` for a
/// [`SocketHandler`] impl that has `self` in scope. When `$self.session_ulid()`
/// returns `None` (e.g. the raw [`TcpStream`] impl that carries no session
/// context) the ULID slot is rendered as `-` so the column layout stays
/// stable across sessionless plumbing. The `[ulid - - -]` context comes first
/// to stay aligned with `MUX-*`, `PIPE` and `RUSTLS` log lines. Colour scheme
/// comes from [`sozu_command::logging::ansi_palette`] — single source of
/// truth for every `log_*_context!` macro in the proxy.
///
/// `peer` is [`SocketHandler::peer_addr`] — the address the handler cached,
/// falling back to a live `getpeername(2)` only where it has none. Same
/// preference order, and now the same single source, as
/// [`log_socket_module_prefix`] and the `MUX-*` contexts, so every rendering
/// of one connection names one host.
///
/// Spelled as a fully-qualified trait call to foreclose a latent hazard, not
/// to fix a present one: `$self.peer_addr()` compiles clean today and passes
/// every test, because no current expansion sits on a raw [`TcpStream`] — all
/// of them are in `impl SocketHandler for FrontRustls`, which has no inherent
/// `peer_addr` for method resolution to prefer. Add one on a bare `mio`
/// [`TcpStream`] and the unqualified form silently picks mio's inherent
/// method instead of this trait's: measured, that slot renders
/// `peer=Ok(127.0.0.1:33587)` beside `local=Some(..)` — an `io::Result` where
/// every other expansion renders an `Option`. The qualification makes that
/// impossible rather than merely unlikely.
///
/// `local`, `rtt`, `state` render per [`log_socket_module_prefix`]'s
/// description; `local` stays a live `getsockname(2)`, which survives what
/// `getpeername(2)` does not.
macro_rules! log_socket_context {
    ($self:expr) => {{
        let (open, reset, grey, gray, white) = ansi_palette();
        let ulid = match $self.session_ulid() {
            Some(ulid) => ulid.to_string(),
            None => "-".to_string(),
        };
        let snapshot = crate::socket::stats::socket_snapshot($self.socket_ref());
        let rtt = snapshot.as_ref().map(|s| s.rtt);
        let state = snapshot.as_ref().map(|s| s.state);
        format!(
            "[{ulid} - - -]\t{open}SOCKET{reset}\t{grey}Session{reset}({gray}peer{reset}={white}{peer:?}{reset}, {gray}local{reset}={white}{local:?}{reset}, {gray}rtt{reset}={white}{rtt:?}{reset}, {gray}state{reset}={white}{state:?}{reset}, {gray}protocol{reset}={white}{protocol:?}{reset})\t >>>",
            open = open,
            reset = reset,
            grey = grey,
            gray = gray,
            white = white,
            ulid = ulid,
            peer = crate::socket::SocketHandler::peer_addr($self),
            local = $self.socket_ref().local_addr().ok(),
            rtt = rtt,
            state = state,
            protocol = $self.protocol(),
        )
    }};
}

/// Module-level socket log prefix used from free functions (e.g. the shared
/// `tcp_socket_*` helpers) where `self` is not in scope but the caller can
/// still thread a session `Ulid`, a cached peer address, and the underlying
/// [`TcpStream`] through as parameters. Renders the same
/// `[<ulid> - - -]\tSOCKET\tSession(peer=..., local=..., rtt=..., state=..., protocol=Tcp)\t >>>`
/// prefix as [`log_socket_context!`]; colour scheme via
/// [`sozu_command::logging::ansi_palette`].
///
/// Per-slot semantics:
///
/// - `peer` — prefers the caller-supplied `configured_peer` (cached at
///   [`SessionTcpStream`] construction, immune to ENOTCONN on a socket that
///   failed an asynchronous `connect()`) and falls back to a live
///   `getpeername(2)` lookup when no cache was provided.
/// - `local` — `getsockname(2)`, stays valid across failed connects.
/// - `rtt` / `state` — a single `getsockopt(TCP_INFO)` call via
///   [`stats::socket_snapshot`]; both render as `None` on an FSM state
///   where the kernel rejects the call. `state="SYN_SENT"` is the
///   clearest signal for a failed outbound `connect()`.
/// - `protocol` — hardcoded to `Tcp` (raw-TCP helpers only).
fn log_socket_module_prefix(
    stream: &TcpStream,
    session_ulid: Option<Ulid>,
    configured_peer: Option<SocketAddr>,
) -> String {
    let (open, reset, grey, gray, white) = ansi_palette();
    let ulid = match session_ulid {
        Some(ulid) => ulid.to_string(),
        None => "-".to_string(),
    };
    let snapshot = crate::socket::stats::socket_snapshot(stream);
    let rtt = snapshot.as_ref().map(|s| s.rtt);
    let state = snapshot.as_ref().map(|s| s.state);
    format!(
        "[{ulid} - - -]\t{open}SOCKET{reset}\t{grey}Session{reset}({gray}peer{reset}={white}{peer:?}{reset}, {gray}local{reset}={white}{local:?}{reset}, {gray}rtt{reset}={white}{rtt:?}{reset}, {gray}state{reset}={white}{state:?}{reset}, {gray}protocol{reset}={white}Tcp{reset})\t >>>",
        peer = configured_peer.or_else(|| stream.peer_addr().ok()),
        local = stream.local_addr().ok(),
    )
}

/// Body of [`tcp_socket_read`], generic over the transport so tests can count
/// every read a call issues against it.
///
/// One `read` per call. A stream socket that answers fewer bytes than it was
/// offered has emptied its receive queue: `man 7 epoll` (man-pages 6.13, the
/// EPOLLET question) — "if you call read(2) by asking to read a certain
/// amount of data and read(2) returns a lower number of bytes, you can be sure
/// of having exhausted the read I/O space for the file descriptor". A second
/// `recv(2)` could only answer EAGAIN, so the call stops there, as HAProxy
/// (`src/raw_sock.c`, `fd_cant_recv` when `ret < try`) and tokio
/// (`tokio/src/io/poll_evented.rs`, `clear_readiness` when `0 < n < len`) do.
///
/// One exception to that guarantee: TCP urgent data. `recv` stops before an
/// urgent (out-of-band) mark even when bytes are queued behind it, so a short
/// read does NOT prove the queue empty while such a mark is pending. Measured
/// on a loopback socket: `AAAA`, `send(B, MSG_OOB)`, `CCCC` raise one edge,
/// this read answers `(4, WouldBlock)` with `AAAA`, no further event comes,
/// and `CCCC` stays in the kernel until the peer sends again. A stream that
/// carries OOB data (telnet, rlogin, FTP `ABOR`) can therefore stall until the
/// peer's next send. HAProxy and tokio behave the same, and sozu never read
/// out-of-band data before either.
///
/// Readiness contract:
///
/// - a short read answers `(n, WouldBlock)`, exactly what the previous loop
///   answered one EAGAIN later; the caller drops READABLE, and mio registers
///   every socket edge-triggered (`mio/src/sys/unix/selector/epoll.rs`,
///   `EPOLLET`), where "an event will be generated upon each receipt of a
///   chunk of data" (`man 7 epoll`), so bytes arriving after this `recv`
///   raise the next event;
/// - a read that fills `buf` is not a short read: it answers `(n, Continue)`
///   and READABLE stays, because more may be queued;
/// - EOF behind the data is left for the next call. A FIN that was already
///   signalled comes with no further edge, so a caller that saw HUP must keep
///   READABLE to read it: `update_readiness_after_read` in the mux does.
///
/// A read the kernel interrupted (`EINTR`) is retried at once, as
/// [`flush_tls`] does for writes: it says nothing about the socket.
///
/// The returned error, when there is one, is a transport failure the caller
/// logs with its socket context; the result is then [`SocketResult::Error`].
fn plain_socket_read<R: Read>(
    stream: &mut R,
    buf: &mut [u8],
) -> (usize, SocketResult, Option<std::io::Error>) {
    // An empty slice reads nothing and says nothing about the socket; callers
    // guard against it, and `(0, Continue)` is what they expect back.
    if buf.is_empty() {
        return (0, SocketResult::Continue, None);
    }
    let read = loop {
        match stream.read(buf) {
            Err(e) if e.kind() == ErrorKind::Interrupted => continue,
            read => break read,
        }
    };
    match read {
        Ok(0) => (0, SocketResult::Closed, None),
        Ok(sz) => {
            // `read` cannot report more bytes than the slice it was given.
            debug_assert!(
                sz <= buf.len(),
                "read reported {sz} bytes into a {}-byte slice",
                buf.len()
            );
            if sz == buf.len() {
                (sz, SocketResult::Continue, None)
            } else {
                (sz, SocketResult::WouldBlock, None)
            }
        }
        Err(e) => match e.kind() {
            ErrorKind::WouldBlock => (0, SocketResult::WouldBlock, None),
            // Treat `ConnectionRefused` as a closed socket, mirroring the
            // write path. On Linux a failed asynchronous `connect()`
            // surfaces as `ECONNREFUSED` on the first read; it is
            // operationally identical to any other benign peer-initiated
            // close and does not warrant a log line on every backend
            // that happens to be down.
            //
            // A reset is still a close, but it is counted: clients that abort
            // instead of finishing cleanly are an operational signal, not an
            // error (sozu-proxy/sozu#434). The kernel reports `ECONNRESET` to
            // the first syscall after the RST only; later reads see EOF and
            // later writes `EPIPE`, so each reset is counted once.
            ErrorKind::ConnectionReset | ErrorKind::ConnectionAborted => {
                incr!(names::tcp::READ_RESET);
                (0, SocketResult::Closed, None)
            }
            ErrorKind::BrokenPipe | ErrorKind::ConnectionRefused => (0, SocketResult::Closed, None),
            _ => (0, SocketResult::Error, Some(e)),
        },
    }
}

/// Shared read/write/vectored-write logic used by both
/// [`impl SocketHandler for TcpStream`] and
/// [`impl SocketHandler for SessionTcpStream`]. Free-function entry point:
/// `self` is out of scope here, so error logs use [`log_socket_module_prefix`]
/// which renders the same `Session(peer, rtt, protocol)` context as
/// [`log_socket_context!`] by reading from the `stream` + `session_ulid`
/// parameters threaded through each helper. The read itself is
/// [`plain_socket_read`].
fn tcp_socket_read(
    stream: &mut TcpStream,
    buf: &mut [u8],
    session_ulid: Option<Ulid>,
    configured_peer: Option<SocketAddr>,
) -> (usize, SocketResult) {
    let (size, result, fault) = plain_socket_read(stream, buf);
    if let Some(e) = fault {
        match e.kind() {
            // Noisy-expected transport failures: backend unreachable,
            // TCP_USER_TIMEOUT expiry, post-close reads. Keep a log line
            // so operators can still trend the rate, but `warn!` — this
            // is reality-at-scale, not a sozu invariant break.
            ErrorKind::HostUnreachable
            | ErrorKind::NetworkUnreachable
            | ErrorKind::TimedOut
            | ErrorKind::NotConnected => warn!(
                "{} socket_read error={:?}",
                log_socket_module_prefix(stream, session_ulid, configured_peer),
                e
            ),
            // Genuinely loud variants (`PermissionDenied`, `AddrNotAvailable`,
            // `InvalidInput`/`Data`, …) and the unknown catch-all stay at
            // `error!` so operators keep paging on real misconfig.
            _ => error!(
                "{} socket_read error={:?}",
                log_socket_module_prefix(stream, session_ulid, configured_peer),
                e
            ),
        }
    }
    (size, result)
}

/// A failure [`plain_socket_write`] or [`plain_socket_write_vectored`] hit,
/// handed back rather than logged so [`tcp_socket_write`] and
/// [`tcp_socket_write_vectored`] render it with their socket context, as
/// [`tcp_socket_read`] does for [`plain_socket_read`]. Its counters are
/// already bumped.
#[derive(Debug)]
enum PlainWriteFault {
    /// The write loop ran [`MAX_LOOP_ITERATIONS`] times without settling.
    LoopLimit,
    /// The socket failed with something other than `WouldBlock`, a reset, a
    /// broken pipe, a refused connection or an interruption.
    Io(std::io::Error),
}

/// Map a failed plain TCP write to its result, bumping its counters. A peer
/// reset (see [`plain_socket_read`]) keeps counting under `tcp.write.error`
/// as well, as it did before `tcp.write.reset`.
fn plain_write_error(e: std::io::Error) -> (SocketResult, Option<PlainWriteFault>) {
    match e.kind() {
        ErrorKind::WouldBlock => (SocketResult::WouldBlock, None),
        ErrorKind::ConnectionReset | ErrorKind::ConnectionAborted => {
            incr!(names::tcp::WRITE_RESET);
            incr!(names::tcp::WRITE_ERROR);
            (SocketResult::Closed, None)
        }
        ErrorKind::BrokenPipe | ErrorKind::ConnectionRefused => {
            incr!(names::tcp::WRITE_ERROR);
            (SocketResult::Closed, None)
        }
        _ => {
            incr!(names::tcp::WRITE_ERROR);
            (SocketResult::Error, Some(PlainWriteFault::Io(e)))
        }
    }
}

/// Body of [`tcp_socket_write`], generic over the transport so tests can
/// script what each write answers.
///
/// A write the kernel interrupted (`EINTR`) is retried at once, as
/// [`flush_tls`] and std's `write_all` do: it says nothing about the socket,
/// so it must neither end the write as `WouldBlock`, which no edge would
/// follow, nor as `Error`, which closes the session. The retry counts against
/// [`MAX_LOOP_ITERATIONS`] like every other pass of the loop.
fn plain_socket_write<W: Write>(
    stream: &mut W,
    buf: &[u8],
) -> (usize, SocketResult, Option<PlainWriteFault>) {
    let mut size = 0usize;
    let mut counter = 0;
    loop {
        counter += 1;
        if counter > MAX_LOOP_ITERATIONS {
            incr!(names::socket::WRITE_INFINITE_LOOP_ERROR);
            return (size, SocketResult::Error, Some(PlainWriteFault::LoopLimit));
        }
        // Loop invariant: the cursor never overshoots the buffer, so the
        // `&buf[size..]` slice below can never panic on a bad offset.
        debug_assert!(
            size <= buf.len(),
            "write cursor {size} overran buffer len {} (would slice out of bounds)",
            buf.len()
        );
        if size == buf.len() {
            return (size, SocketResult::Continue, None);
        }
        match stream.write(&buf[size..]) {
            Ok(0) => return (size, SocketResult::Continue, None),
            Ok(sz) => {
                // `write` cannot report more bytes than the slice it was given.
                debug_assert!(
                    sz <= buf.len() - size,
                    "write reported {sz} bytes from a {}-byte remaining slice",
                    buf.len() - size
                );
                size += sz;
            }
            Err(e) if e.kind() == ErrorKind::Interrupted => {}
            Err(e) => {
                let (result, fault) = plain_write_error(e);
                return (size, result, fault);
            }
        }
    }
}

/// Body of [`tcp_socket_write_vectored`], generic over the transport so tests
/// can script what each write answers. One `writev(2)`, retried at once when
/// the kernel interrupts it (`EINTR`), as [`plain_socket_write`] retries.
fn plain_socket_write_vectored<W: Write>(
    stream: &mut W,
    bufs: &[std::io::IoSlice],
) -> (usize, SocketResult, Option<PlainWriteFault>) {
    let written = loop {
        match stream.write_vectored(bufs) {
            Err(e) if e.kind() == ErrorKind::Interrupted => continue,
            written => break written,
        }
    };
    match written {
        Ok(sz) => {
            // `write_vectored` cannot report more bytes than the slices held.
            debug_assert!(
                sz <= bufs.iter().map(|b| b.len()).sum::<usize>(),
                "write_vectored reported {sz} bytes from {}-byte slices",
                bufs.iter().map(|b| b.len()).sum::<usize>()
            );
            (sz, SocketResult::Continue, None)
        }
        Err(e) => {
            let (result, fault) = plain_write_error(e);
            (0, result, fault)
        }
    }
}

/// Log a [`PlainWriteFault`] with the socket context of [`tcp_socket_read`].
fn log_plain_write_fault(
    stream: &TcpStream,
    session_ulid: Option<Ulid>,
    configured_peer: Option<SocketAddr>,
    fault: PlainWriteFault,
) {
    match fault {
        PlainWriteFault::LoopLimit => error!(
            "{} MAX_LOOP_ITERATION reached in TcpStream::socket_write",
            log_socket_module_prefix(stream, session_ulid, configured_peer)
        ),
        PlainWriteFault::Io(e) => match e.kind() {
            // Noisy-expected transport failures (see `tcp_socket_read` for
            // rationale): `warn!`, the `tcp.write.error` counter still bumped
            // so rate-based dashboards stay accurate.
            ErrorKind::HostUnreachable
            | ErrorKind::NetworkUnreachable
            | ErrorKind::TimedOut
            | ErrorKind::NotConnected => warn!(
                "{} socket_write error={:?}",
                log_socket_module_prefix(stream, session_ulid, configured_peer),
                e
            ),
            //FIXME: timeout and other common errors should be sent up
            _ => error!(
                "{} socket_write error={:?}",
                log_socket_module_prefix(stream, session_ulid, configured_peer),
                e
            ),
        },
    }
}

fn tcp_socket_write(
    stream: &mut TcpStream,
    buf: &[u8],
    session_ulid: Option<Ulid>,
    configured_peer: Option<SocketAddr>,
) -> (usize, SocketResult) {
    let (size, result, fault) = plain_socket_write(stream, buf);
    if let Some(fault) = fault {
        log_plain_write_fault(stream, session_ulid, configured_peer, fault);
    }
    (size, result)
}

fn tcp_socket_write_vectored(
    stream: &mut TcpStream,
    bufs: &[std::io::IoSlice],
    session_ulid: Option<Ulid>,
    configured_peer: Option<SocketAddr>,
) -> (usize, SocketResult) {
    let (size, result, fault) = plain_socket_write_vectored(stream, bufs);
    if let Some(fault) = fault {
        log_plain_write_fault(stream, session_ulid, configured_peer, fault);
    }
    (size, result)
}

impl SocketHandler for TcpStream {
    fn socket_read(&mut self, buf: &mut [u8]) -> (usize, SocketResult) {
        tcp_socket_read(self, buf, None, None)
    }

    fn socket_write(&mut self, buf: &[u8]) -> (usize, SocketResult) {
        tcp_socket_write(self, buf, None, None)
    }

    fn socket_write_vectored(&mut self, bufs: &[std::io::IoSlice]) -> (usize, SocketResult) {
        tcp_socket_write_vectored(self, bufs, None, None)
    }

    fn socket_ref(&self) -> &TcpStream {
        self
    }

    fn socket_mut(&mut self) -> &mut TcpStream {
        self
    }

    /// No context to cache: a bare stream is all there is, so this is the live
    /// lookup and renders `None` once the peer has reset. Call sites that need
    /// a surviving address use [`SessionTcpStream`] or [`FrontRustls`].
    /// Fully-qualified so it reads as the inherent `mio` method, not a
    /// recursive call into the trait method being defined.
    fn peer_addr(&self) -> Option<SocketAddr> {
        TcpStream::peer_addr(self).ok()
    }

    fn protocol(&self) -> TransportProtocol {
        TransportProtocol::Tcp
    }

    fn read_error(&self) {
        incr!(names::tcp::READ_ERROR);
    }

    fn write_error(&self) {
        incr!(names::tcp::WRITE_ERROR);
    }
}

/// [`TcpStream`] wrapped with the owning session's ULID. Exists so plain-TCP
/// frontends and backends inside the mux stack can prefix SOCKET-layer error
/// logs with `[<session_ulid> - - -]`, matching what TLS-wrapped frontends
/// already do via [`FrontRustls::session_ulid`].
///
/// The inner [`TcpStream`] is exposed directly so mio registration sites can
/// borrow it as-is; the outer type only participates in the [`SocketHandler`]
/// trait dispatch.
#[derive(Debug)]
pub struct SessionTcpStream {
    pub stream: TcpStream,
    pub session_ulid: Ulid,
    /// Peer address cached at construction. For backend-facing sockets
    /// (created from a nonblocking `connect()` in `Mux::dial_backend`) this is
    /// the cluster-configured backend address — reliable across ENOTCONN
    /// after a failed handshake, which is the sharp case that motivates the
    /// cache. For frontend-facing sockets constructed from an accepted
    /// `TcpStream`, this is the client's peer address — identical to what a
    /// live `getpeername(2)` would return, but threaded through the same
    /// plumbing for uniformity. Used as the preferred source of truth for
    /// the `peer=` slot in `log_socket_module_prefix`, falling back to a
    /// live lookup when `None`.
    pub configured_peer: Option<SocketAddr>,
}

impl SessionTcpStream {
    pub fn new(stream: TcpStream, session_ulid: Ulid, configured_peer: Option<SocketAddr>) -> Self {
        Self {
            stream,
            session_ulid,
            configured_peer,
        }
    }
}

impl SocketHandler for SessionTcpStream {
    fn socket_read(&mut self, buf: &mut [u8]) -> (usize, SocketResult) {
        tcp_socket_read(
            &mut self.stream,
            buf,
            Some(self.session_ulid),
            self.configured_peer,
        )
    }

    fn socket_write(&mut self, buf: &[u8]) -> (usize, SocketResult) {
        tcp_socket_write(
            &mut self.stream,
            buf,
            Some(self.session_ulid),
            self.configured_peer,
        )
    }

    fn socket_write_vectored(&mut self, bufs: &[std::io::IoSlice]) -> (usize, SocketResult) {
        tcp_socket_write_vectored(
            &mut self.stream,
            bufs,
            Some(self.session_ulid),
            self.configured_peer,
        )
    }

    fn socket_ref(&self) -> &TcpStream {
        &self.stream
    }

    fn socket_mut(&mut self) -> &mut TcpStream {
        &mut self.stream
    }

    /// Same preference order as `log_socket_module_prefix` (private, so not
    /// linked), which this type's
    /// I/O helpers already feed `configured_peer` into — one address for both
    /// the `SOCKET` and `MUX-*` renderings of the same connection.
    fn peer_addr(&self) -> Option<SocketAddr> {
        self.configured_peer
            .or_else(|| self.stream.peer_addr().ok())
    }

    fn protocol(&self) -> TransportProtocol {
        TransportProtocol::Tcp
    }

    fn read_error(&self) {
        incr!(names::tcp::READ_ERROR);
    }

    fn write_error(&self) {
        incr!(names::tcp::WRITE_ERROR);
    }

    fn session_ulid(&self) -> Option<Ulid> {
        Some(self.session_ulid)
    }
}

pub struct FrontRustls {
    pub stream: TcpStream,
    pub session: ServerConnection,
    /// Peer sent a graceful FIN on the read side (`read()` returned `Ok(0)`).
    /// We can no longer receive plaintext, but may still have rustls-buffered
    /// records to flush on the write side — do NOT abort pending writes.
    pub peer_disconnected: bool,
    /// The TCP channel is dead: the peer reset the connection
    /// (RST/ConnectionAborted/BrokenPipe), or a write answered
    /// [`SocketResult::Error`] — a socket error other than `WouldBlock`
    /// (an `Interrupted` write is retried in `flush_tls` and never gets
    /// here), a rustls writer failure, or the write loop's budget. Further
    /// writes are pointless and short-circuit, and
    /// [`SocketHandler::socket_wants_write`] stops reporting the records
    /// rustls still holds: no write can deliver them, so they must neither
    /// keep the session open nor wait for a WRITABLE edge that will not come.
    pub peer_reset: bool,
    /// `process_new_packets` failed on this connection: a corrupt record, a
    /// fatal alert, a protocol violation. rustls keeps that error for good,
    /// so from then on [`SocketHandler::socket_read`] answers `(0, Error)`
    /// at once, without serving plaintext rustls decrypted before the
    /// failure. See `rustls_socket_read`.
    pub tls_fatal: bool,
    /// Connection/session ULID propagated from the enclosing mux session.
    /// Rendered into SOCKET-layer error logs via [`Self::session_ulid`].
    pub session_ulid: Ulid,
    /// Peer address snapshot, same role as [`SessionTcpStream::configured_peer`]
    /// and same name so the two handlers read alike. Seeded from
    /// `HttpsSession::peer_address`, which is the PROXY-advertised source on an
    /// expect-proxy frontend and the accepted socket's peer otherwise — NOT
    /// from `stream.peer_addr()`, which would be the load balancer in the first
    /// case and would die with `ENOTCONN` in both. Read through
    /// [`SocketHandler::peer_addr`].
    pub configured_peer: Option<SocketAddr>,
    /// What the read path may assume about `stream`'s receive queue from one
    /// [`SocketHandler::socket_read`] call to the next. See [`RecvMemory`].
    pub recv_memory: RecvMemory,
}

impl FrontRustls {
    /// Record that the event loop delivered `events` for `stream`: whatever a
    /// `recv` proved before, bytes may have arrived since. The HTTPS session
    /// calls it for every event on its frontend token, whatever its state
    /// (`HttpsSession::update_readiness`, `lib/src/https.rs`), which is the
    /// only way events reach a session (`Server::ready`, `lib/src/server.rs`).
    pub fn readiness_delivered(&mut self, events: Ready) {
        self.recv_memory.event_delivered(events);
    }
}

/// What `rustls_socket_read` may assume about the receive queue between two
/// calls.
///
/// The H2 read path makes one call per frame header and one per payload
/// (`lib/src/protocol/mux/h2.rs`, `readable_inner`), so the frames of one
/// record are served by several calls: the first one's `recv` is short, the
/// rest come from rustls's plaintext buffer. Without a memory, the call that
/// finds that buffer empty issued one more `recv`, which could only answer
/// EAGAIN — once per request on a multiplexed connection (#1609).
///
/// `drained` carries the proof across calls, and is safe for as long as no
/// readiness event has been delivered since it was set. mio registers every
/// socket edge-triggered (`mio/src/sys/unix/selector/epoll.rs`, `EPOLLET`),
/// and "an event will be generated upon each receipt of a chunk of data"
/// (`man 7 epoll`), so a byte that arrives after the proof either raises an
/// event still pending in epoll — the next `epoll_wait` delivers it and
/// re-arms READABLE — or one already delivered, which cleared `drained`. A
/// delivery that lands while the mux still holds READABLE changes nothing the
/// mux can see, which is why the memory is cleared at delivery rather than
/// inferred from READABLE.
///
/// `hup_seen` turns the memory off for good once HUP or ERROR is delivered.
/// A FIN that arrived with the data raised its edge already, and no later
/// event will announce the EOF behind a short read; the mux keeps READABLE
/// for it (`update_readiness_after_read`), and the next call must read it.
///
/// The TCP urgent data reserve of `plain_socket_read` applies unchanged: a
/// `recv` stopped short by an urgent mark proves nothing, and the stream
/// stalls until the peer's next send, as it does within a call since #1606.
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
pub struct RecvMemory {
    /// A `recv` answered short or EAGAIN, and no event has been delivered
    /// since.
    drained: bool,
    /// HUP or ERROR has been delivered.
    hup_seen: bool,
}

impl RecvMemory {
    /// The memory a session starts with once its handshake is over. `drained`
    /// is what the handshake proved: a `recv` that answered short or EAGAIN
    /// dropped READABLE from its event, and only a delivered event with
    /// READABLE put it back (`handshake_read`, `lib/src/protocol/rustls.rs`).
    /// A handshake closes on HUP or ERROR, so none was seen.
    pub fn after_handshake(drained: bool) -> Self {
        RecvMemory {
            drained,
            hup_seen: false,
        }
    }

    fn event_delivered(&mut self, events: Ready) {
        self.drained = false;
        if events.is_hup() || events.is_error() {
            self.hup_seen = true;
        }
    }
}

/// One `write_tls` of `session` into `stream`: at most one `writev(2)`, and
/// none when rustls holds no record (`ChunkVecBuffer::write_to` returns
/// `Ok(0)` before touching the socket, `rustls-0.23.45/src/vecbuf.rs`).
///
/// Every `FrontRustls` write goes through here so tests can count the writes
/// that reach the socket ([`tls_writes`]).
///
/// It is also the one place a TLS frontend's write can observe a peer reset,
/// so `rustls.write.reset` is counted here rather than at each caller's
/// error arm (sozu-proxy/sozu#434); the callers' `peer_reset` then stops
/// every later write before it reaches the socket, so a reset counts once.
/// The callers still count it under `rustls.write.error` as well.
///
/// A write the kernel interrupted (`EINTR`) is retried at once, as the
/// relay (`lib/src/protocol/proxy_protocol/relay.rs`), the UDP path
/// (`lib/src/udp.rs`) and the plain TCP paths ([`plain_socket_read`],
/// [`plain_socket_write`], [`plain_socket_write_vectored`]) do: it says
/// nothing about the socket, so it must
/// neither end the write as `WouldBlock`, which no edge would follow, nor
/// as `Error`, which marks the channel dead.
fn flush_tls(
    session: &mut ServerConnection,
    stream: &mut impl std::io::Write,
) -> std::io::Result<usize> {
    #[cfg(test)]
    if session.wants_write() {
        TLS_WRITES.with(|writes| writes.set(writes.get() + 1));
    }
    let written = loop {
        match session.write_tls(stream) {
            Err(e) if e.kind() == ErrorKind::Interrupted => continue,
            written => break written,
        }
    };
    if let Err(e) = &written
        && matches!(
            e.kind(),
            ErrorKind::ConnectionReset | ErrorKind::ConnectionAborted
        )
    {
        incr!(names::rustls::WRITE_RESET);
    }
    written
}

#[cfg(test)]
thread_local! {
    static TLS_WRITES: std::cell::Cell<usize> = const { std::cell::Cell::new(0) };
}

/// How many `write_tls` calls holding at least one record the calling thread
/// has issued through [`flush_tls`] so far.
#[cfg(test)]
pub(crate) fn tls_writes() -> usize {
    TLS_WRITES.with(std::cell::Cell::get)
}

impl std::fmt::Debug for FrontRustls {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("FrontRustls")
            .field("stream", &self.stream)
            .finish_non_exhaustive()
    }
}

/// A failure [`rustls_socket_read`] hit, handed back rather than logged so the
/// caller can render it with its full socket context: the helper borrows the
/// stream mutably for the whole pass, which is exactly what
/// `log_socket_context!` needs to read.
#[derive(Debug)]
enum RustlsReadFault {
    /// The pass ran [`MAX_LOOP_ITERATIONS`] times without settling.
    LoopLimit,
    /// `read_tls` failed with something other than `WouldBlock`, a reset, an
    /// interruption (retried), or rustls's own "plaintext buffer full".
    ReadTls(std::io::Error),
    /// `process_new_packets` rejected what `read_tls` delivered.
    ProcessPackets(rustls::Error),
    /// The plaintext reader failed with something other than `WouldBlock` or
    /// a reset.
    Plaintext(std::io::Error),
}

/// A [`Read`] adapter that records whether its last read returned fewer bytes
/// than it was offered.
///
/// `ServerConnection::read_tls` issues exactly one `read` into its deframer
/// buffer (`rustls-0.23.45/src/msgs/deframer/buffers.rs:208`) and returns only
/// the byte count, not the length it offered, so a short read is invisible
/// from its return value. An `Ok(0)` is EOF, not a short read, and an error is
/// neither.
///
/// Two callers wrap `read_tls` in it: [`rustls_socket_read`] once the session
/// is established, and `handshake_read` (`lib/src/protocol/rustls.rs`) while
/// the handshake runs.
pub(crate) struct ShortReadProbe<'a, R> {
    inner: &'a mut R,
    short: bool,
}

impl<'a, R> ShortReadProbe<'a, R> {
    pub(crate) fn new(inner: &'a mut R) -> Self {
        ShortReadProbe {
            inner,
            short: false,
        }
    }

    /// Whether the last read answered fewer bytes than it was offered, which
    /// on a stream socket proves its receive queue empty (see
    /// [`plain_socket_read`] for the one exception, TCP urgent data).
    pub(crate) fn was_short(&self) -> bool {
        self.short
    }
}

impl<R: Read> Read for ShortReadProbe<'_, R> {
    fn read(&mut self, buf: &mut [u8]) -> std::io::Result<usize> {
        let read = self.inner.read(buf)?;
        self.short = 0 < read && read < buf.len();
        Ok(read)
    }
}

/// Body of [`FrontRustls::socket_read`], generic over the transport so tests
/// can count every read the pass issues against it.
///
/// Each pass drains the plaintext rustls already decrypted, and only then —
/// if the caller's buffer is still not full — calls `read_tls` (one `recv(2)`)
/// and `process_new_packets`. That order is what tokio-rustls follows
/// (`tokio-rustls-0.26.4/src/common/mod.rs:198-212`: `read_tls` only while
/// `wants_read()`, then the plaintext), and what OpenSSL gives HAProxy for
/// free: `SSL_read` serves an already-decrypted record without touching the
/// socket.
///
/// Readiness contract, unchanged by that order:
///
/// - every `recv` that answers EAGAIN ends the call with
///   [`SocketResult::WouldBlock`], because the drain ran first and so the
///   buffer is not full; the mux then drops READABLE until the next mio
///   event (`update_readiness`), which is the edge-triggered re-arm;
/// - so does a `read_tls` whose `recv` answered fewer bytes than rustls
///   offered (4096, `rustls-0.23.45/src/msgs/deframer/buffers.rs:220`): the
///   receive queue is empty, as for [`plain_socket_read`], so the pass makes
///   no second `recv` that could only answer EAGAIN. It still processes what
///   arrived and drains the plaintext, and answers `WouldBlock` only when
///   the buffer is still not full, and `Closed` when that arrival carried
///   `close_notify`. [`ShortReadProbe`] sees the offered length, which
///   `read_tls` does not return;
/// - [`SocketResult::Continue`] with a full buffer means "more may be
///   buffered", so READABLE stays and the next call drains before it reads;
/// - a `read_tls` that fills what rustls offered but completes no record
///   produces no plaintext, so the pass loops and reads again until a short
///   read, EAGAIN, EOF or a full buffer;
/// - one record can hold several H2 frames: the later calls are served from
///   the plaintext buffer without a `recv`, and the call that empties that
///   buffer makes no `recv` either when an earlier call's `recv` answered
///   short or EAGAIN and no readiness event was delivered since: `memory`
///   carries that proof ([`RecvMemory`]), so the call answers `WouldBlock`
///   without touching the socket. Once HUP or ERROR has been delivered the
///   memory is off, and a call that finds the plaintext empty reads.
///
/// [`SocketResult::Error`] from `process_new_packets` is sticky: the failing
/// pass still reports the plaintext it had already copied into `buf` before
/// reaching `read_tls` (a `(n, Error)` return), but it sets `tls_fatal`, and
/// every later pass answers `(0, Error)` without draining anything rustls
/// decrypted before the failure.
///
/// EOF and `close_notify` are reported as [`SocketResult::Closed`] by the
/// first call that finds no plaintext left to deliver, never behind it: a
/// peer's last frames are handed over before the close. A call the buffered
/// plaintext can satisfy completely returns `Continue` without looking at
/// the socket, so a FIN that is already queued is seen one call later than
/// when every call began with `read_tls`. A TCP FIN queued behind a short
/// read is likewise seen by the next call, which the mux makes once it has
/// seen HUP (`update_readiness_after_read`), and which reads because that HUP
/// turned the memory off.
fn rustls_socket_read<R: Read>(
    session: &mut ServerConnection,
    stream: &mut R,
    buf: &mut [u8],
    peer_disconnected: &mut bool,
    peer_reset: &mut bool,
    tls_fatal: &mut bool,
    memory: &mut RecvMemory,
) -> (usize, SocketResult, Option<RustlsReadFault>) {
    // `Error` is sticky. Once `process_new_packets` has failed, plaintext it
    // appended from records that preceded the bad one may still sit in the
    // reader; serving it would hand the mux frames from a session that is
    // already dead, and the mux only drops READABLE on `Error`, so the next
    // peer bytes would be parsed. The failure was logged on the pass that
    // hit it; later passes answer silently.
    if *tls_fatal {
        return (0, SocketResult::Error, None);
    }
    let mut size = 0usize;
    let mut can_read = true;
    // Seeded from the memory: a `recv` in an earlier call may already have
    // proved the queue empty, with no event delivered since.
    let mut drained = memory.drained && !memory.hup_seen;
    let mut is_error = false;
    let mut is_closed = false;
    let mut fault = None;

    let mut counter = 0;
    loop {
        counter += 1;
        if counter > MAX_LOOP_ITERATIONS {
            fault = Some(RustlsReadFault::LoopLimit);
            is_error = true;
            break;
        }

        // Plaintext first: rustls may already hold decrypted bytes from a
        // record an earlier call read — under H2, which asks for one 9-byte
        // frame header and then one payload at a time, the rest of that
        // record's frames. Serving them before `read_tls` is what keeps a
        // call the buffer can satisfy from issuing a `recv(2)` that can only
        // answer EAGAIN. `wants_read()` is false exactly while plaintext is
        // pending or `close_notify` has been received
        // (rustls-0.23.45/src/common_state.rs:674-684); in the second case the
        // reader answers `Ok(0)` and the `read_tls` below reports it.
        while !session.wants_read() {
            match session.reader().read(&mut buf[size..]) {
                Ok(0) => break,
                Ok(sz) => {
                    // The rustls reader cannot return more plaintext than
                    // the remaining slice it was handed.
                    debug_assert!(
                        sz <= buf.len() - size,
                        "rustls reader returned {sz} bytes into a {}-byte remaining slice",
                        buf.len() - size
                    );
                    size += sz;
                }
                Err(e) => match e.kind() {
                    ErrorKind::WouldBlock => {
                        break;
                    }
                    ErrorKind::ConnectionReset
                    | ErrorKind::ConnectionAborted
                    | ErrorKind::BrokenPipe => {
                        is_closed = true;
                        break;
                    }
                    _ => {
                        fault = Some(RustlsReadFault::Plaintext(e));
                        is_error = true;
                        break;
                    }
                },
            }
        }

        // Loop invariant: the plaintext cursor never overshoots the caller's
        // buffer, so the next pass's `&mut buf[size..]` is a valid slice.
        debug_assert!(
            size <= buf.len(),
            "rustls read cursor {size} overran buffer len {} (would slice out of bounds)",
            buf.len()
        );
        if size == buf.len() {
            break;
        }

        if !can_read | is_error | is_closed {
            break;
        }

        // A short or EAGAIN `recv` earlier in this pass — or in an earlier
        // call, with no event delivered since (`RecvMemory`) — emptied the
        // receive queue, so a `read_tls` that would `recv` again can only
        // answer EAGAIN. One that
        // would not — `close_notify` received, which is exactly when
        // `wants_read()` is false with the plaintext drained — still runs:
        // it answers `Ok(0)` without touching the socket
        // (rustls-0.23.45/src/conn.rs:768-770) and reports the close.
        if drained && session.wants_read() {
            can_read = false;
            break;
        }

        // rustls hands an interrupted `recv` (`EINTR`) back from `read_tls`
        // (only its `complete_io` retries it), and it says nothing about the
        // socket, so it is retried at once, as [`plain_socket_read`] does. The
        // probe only records a successful read, so the retry leaves
        // `was_short` exact.
        let mut probe = ShortReadProbe::new(&mut *stream);
        let read = loop {
            match session.read_tls(&mut probe) {
                Err(e) if e.kind() == ErrorKind::Interrupted => continue,
                read => break read,
            }
        };
        drained |= probe.was_short();
        match read {
            Ok(0) => {
                // Graceful FIN on the read side: peer closed its write
                // half. Keep `peer_reset` unset so outbound writes can
                // still flush rustls's buffered records (half-close).
                can_read = false;
                is_closed = true;
                *peer_disconnected = true;
            }
            Ok(_sz) => {}
            Err(e) => match e.kind() {
                ErrorKind::WouldBlock => {
                    can_read = false;
                    drained = true;
                }
                ErrorKind::ConnectionReset
                | ErrorKind::ConnectionAborted
                | ErrorKind::BrokenPipe => {
                    // Full RST/abort: the TCP channel is dead. Mark
                    // `peer_reset` so writes short-circuit (nothing can
                    // reach the peer anymore) but still set
                    // `peer_disconnected` for back-compatible read-side
                    // logic.
                    //
                    // The reset is counted here, the only arm that reads the
                    // socket, and once: `peer_reset` stops every later write
                    // before it reaches the socket (sozu-proxy/sozu#434).
                    // `EPIPE` is not a reset this `recv` observed.
                    if e.kind() != ErrorKind::BrokenPipe {
                        incr!(names::rustls::READ_RESET);
                    }
                    is_closed = true;
                    *peer_disconnected = true;
                    *peer_reset = true;
                }
                // rustls-0.23.45/src/conn.rs:761-766: rustls's 16 KB
                // received_plaintext buffer is full. The drain above empties
                // it before every `read_tls` unless the caller's buffer
                // filled first, which breaks out before reaching here, so
                // this arm is defensive; the next pass drains again.
                ErrorKind::Other => {}
                _ => {
                    fault = Some(RustlsReadFault::ReadTls(e));
                    is_error = true;
                    break;
                }
            },
        }

        if let Err(e) = session.process_new_packets() {
            fault = Some(RustlsReadFault::ProcessPackets(e));
            *tls_fatal = true;
            is_error = true;
            break;
        }
    }

    // Post-condition: we never report more plaintext than the caller asked
    // for, and Error/Closed are mutually exclusive (the loop `break`s on the
    // first one set, so both can never be true on the same pass).
    debug_assert!(
        size <= buf.len(),
        "rustls socket_read returned {size} bytes for a {}-byte buffer",
        buf.len()
    );
    debug_assert!(
        !(is_error && is_closed),
        "rustls socket_read cannot be both Error and Closed"
    );
    // Carried to the next call; `FrontRustls::readiness_delivered` clears it.
    memory.drained = drained;
    let result = if is_error {
        SocketResult::Error
    } else if is_closed {
        SocketResult::Closed
    } else if size == buf.len() {
        // The full requested amount was read (possibly from the rustls
        // plaintext buffer). Report Continue so the caller keeps
        // READABLE in the readiness set — there may be more decrypted
        // data available without a new mio event.
        SocketResult::Continue
    } else if !can_read {
        SocketResult::WouldBlock
    } else {
        SocketResult::Continue
    };
    (size, result, fault)
}

impl SocketHandler for FrontRustls {
    fn socket_read(&mut self, buf: &mut [u8]) -> (usize, SocketResult) {
        let (size, result, fault) = rustls_socket_read(
            &mut self.session,
            &mut self.stream,
            buf,
            &mut self.peer_disconnected,
            &mut self.peer_reset,
            &mut self.tls_fatal,
            &mut self.recv_memory,
        );
        match fault {
            None => {}
            Some(RustlsReadFault::LoopLimit) => {
                error!(
                    "{} MAX_LOOP_ITERATION reached in FrontRustls::socket_read",
                    log_socket_context!(self)
                );
                incr!(names::rustls::READ_INFINITE_LOOP_ERROR);
            }
            Some(RustlsReadFault::ReadTls(e)) => error!(
                "{} could not read TLS stream from socket: {:?}",
                log_socket_context!(self),
                e
            ),
            Some(RustlsReadFault::ProcessPackets(e)) => error!(
                "{} could not process read TLS packets: {:?}",
                log_socket_context!(self),
                e
            ),
            Some(RustlsReadFault::Plaintext(e)) => error!(
                "{} could not read data from TLS stream: {:?}",
                log_socket_context!(self),
                e
            ),
        }
        (size, result)
    }

    /// Keep these two functions structurally symmetric — a divergence
    /// caused the 4.5 MB H2 truncation bug. Tests
    /// `e2e::tests::h2_correctness_tests::*` and
    /// `e2e::tests::h2_tests::test_h2_large_*` are the regression guard.
    fn socket_write(&mut self, buf: &[u8]) -> (usize, SocketResult) {
        // Abort only on a true RST — a FIN on the read side still permits
        // flushing rustls's plaintext buffer (TLS half-close).
        if self.peer_reset {
            return (0, SocketResult::Closed);
        }

        let mut buffered_size = 0usize;
        let mut can_write = true;
        let mut is_error = false;
        let mut is_closed = false;

        let mut counter = 0;
        loop {
            counter += 1;
            if counter > MAX_LOOP_ITERATIONS {
                error!(
                    "{} MAX_LOOP_ITERATION reached in FrontRustls::socket_write",
                    log_socket_context!(self)
                );
                incr!(names::rustls::WRITE_INFINITE_LOOP_ERROR);
                is_error = true;
                break;
            }
            // Loop invariant: the absorbed-plaintext cursor never overshoots the
            // caller's buffer, so `&buf[buffered_size..]` is always a valid slice.
            debug_assert!(
                buffered_size <= buf.len(),
                "rustls write cursor {buffered_size} overran buffer len {} (would slice out of bounds)",
                buf.len()
            );
            if buffered_size == buf.len() {
                break;
            }

            if !can_write | is_error | is_closed {
                break;
            }

            match self.session.writer().write(&buf[buffered_size..]) {
                Ok(0) => {} // zero byte written means that the Rustls buffers are full, we will try to write on the socket and try again
                Ok(sz) => {
                    // rustls cannot absorb more plaintext than the remaining slice.
                    debug_assert!(
                        sz <= buf.len() - buffered_size,
                        "rustls writer absorbed {sz} bytes from a {}-byte remaining slice",
                        buf.len() - buffered_size
                    );
                    buffered_size += sz;
                }
                Err(e) => match e.kind() {
                    ErrorKind::WouldBlock => {
                        // we don't need to do anything, the session will return false in wants_write?
                        //error!("rustls socket_write wouldblock");
                    }
                    ErrorKind::ConnectionReset
                    | ErrorKind::ConnectionAborted
                    | ErrorKind::BrokenPipe => {
                        //FIXME: this should probably not happen here
                        incr!(names::rustls::WRITE_ERROR);
                        is_closed = true;
                        self.peer_reset = true;
                        break;
                    }
                    _ => {
                        error!(
                            "{} could not write data to TLS stream: {:?}",
                            log_socket_context!(self),
                            e
                        );
                        incr!(names::rustls::WRITE_ERROR);
                        is_error = true;
                        break;
                    }
                },
            }

            loop {
                match flush_tls(&mut self.session, &mut self.stream) {
                    Ok(0) => {
                        //can_write = false;
                        break;
                    }
                    Ok(_sz) => {}
                    Err(e) => match e.kind() {
                        ErrorKind::WouldBlock => {
                            can_write = false;
                            break;
                        }
                        ErrorKind::ConnectionReset
                        | ErrorKind::ConnectionAborted
                        | ErrorKind::BrokenPipe => {
                            incr!(names::rustls::WRITE_ERROR);
                            is_closed = true;
                            self.peer_reset = true;
                            break;
                        }
                        _ => {
                            error!(
                                "{} could not write TLS stream to socket: {:?}",
                                log_socket_context!(self),
                                e
                            );
                            incr!(names::rustls::WRITE_ERROR);
                            is_error = true;
                            break;
                        }
                    },
                }
            }
        }

        // Flush any pending TLS records even if no application data was written.
        // This handles the case where h2.rs calls socket_write(&[]) to flush
        // buffered TLS data (e.g. NewSessionTicket, key updates). Without this,
        // the main loop above exits immediately for empty buffers and write_tls
        // is never called.
        if !is_error && !is_closed && can_write && self.session.wants_write() {
            loop {
                match flush_tls(&mut self.session, &mut self.stream) {
                    Ok(0) => break,
                    Ok(_) => {}
                    Err(e) => match e.kind() {
                        ErrorKind::WouldBlock => {
                            can_write = false;
                            break;
                        }
                        ErrorKind::ConnectionReset
                        | ErrorKind::ConnectionAborted
                        | ErrorKind::BrokenPipe => {
                            incr!(names::rustls::WRITE_ERROR);
                            is_closed = true;
                            self.peer_reset = true;
                            break;
                        }
                        _ => {
                            error!(
                                "{} could not flush TLS stream to socket: {:?}",
                                log_socket_context!(self),
                                e
                            );
                            incr!(names::rustls::WRITE_ERROR);
                            is_error = true;
                            break;
                        }
                    },
                }
            }
        }

        // Post-condition: we never report absorbing more plaintext than the
        // caller handed us — over-reporting is exactly the truncation-class bug
        // these two symmetric paths exist to avoid.
        debug_assert!(
            buffered_size <= buf.len(),
            "rustls socket_write reported {buffered_size} bytes for a {}-byte buffer",
            buf.len()
        );
        debug_assert!(
            !(is_error && is_closed),
            "rustls socket_write cannot be both Error and Closed"
        );
        if is_error {
            // Whatever failed — the socket, rustls, or the loop budget — no
            // later write can deliver what rustls holds: see `peer_reset`.
            self.peer_reset = true;
            (buffered_size, SocketResult::Error)
        } else if is_closed {
            (buffered_size, SocketResult::Closed)
        } else if !can_write {
            (buffered_size, SocketResult::WouldBlock)
        } else {
            (buffered_size, SocketResult::Continue)
        }
    }

    /// Write a list of plaintext slices through the rustls session.
    ///
    /// Empty-buffer invariant: callers may legitimately pass `bufs.is_empty()`
    /// or an all-empty slice to request a pure flush pass. In that case
    /// `total_len == 0`, the top-of-loop `buffered_size == total_len` guard
    /// fires immediately after `write_tls` drains any pending TLS records the
    /// session still has buffered (e.g. the remainder of a record split by
    /// the previous call, or `close_notify` output). This mirrors
    /// [`Self::socket_write`]: both entry points must stay structurally
    /// symmetric so that a zero-byte flush never early-returns without giving
    /// rustls a chance to emit bytes.
    ///
    /// Keep these two functions structurally symmetric — a divergence
    /// caused the 4.5 MB H2 truncation bug. Tests
    /// `e2e::tests::h2_correctness_tests::*` and
    /// `e2e::tests::h2_tests::test_h2_large_*` are the regression guard.
    fn socket_write_vectored(&mut self, bufs: &[std::io::IoSlice]) -> (usize, SocketResult) {
        if self.peer_reset {
            return (0, SocketResult::Closed);
        }

        let total_len: usize = bufs.iter().map(|b| b.len()).sum();
        let mut buffered_size = 0usize;
        let mut can_write = true;
        let mut is_error = false;
        let mut is_closed = false;

        let mut counter = 0;
        loop {
            counter += 1;
            if counter > MAX_LOOP_ITERATIONS {
                error!(
                    "{} MAX_LOOP_ITERATION reached in FrontRustls::socket_write_vectored",
                    log_socket_context!(self)
                );
                incr!(names::rustls::WRITE_INFINITE_LOOP_ERROR);
                is_error = true;
                break;
            }
            // Loop invariant: the absorbed-plaintext cursor never overshoots the
            // summed slice length we computed up front (mirrors the scalar path).
            debug_assert!(
                buffered_size <= total_len,
                "rustls vectored write cursor {buffered_size} overran total slice len {total_len}"
            );
            if buffered_size == total_len {
                break;
            }

            if !can_write | is_error | is_closed {
                break;
            }

            // rustls's Writer does not expose a "write from offset across slices"
            // helper, so we push plaintext once and then drain via write_tls.
            // If rustls only partially absorbs the slices, we break and return
            // the partial count so the caller can advance its buffers and retry.
            if buffered_size == 0 {
                match self.session.writer().write_vectored(bufs) {
                    Ok(0) => {}
                    Ok(sz) => {
                        // rustls cannot absorb more plaintext than the slices held.
                        debug_assert!(
                            sz <= total_len,
                            "rustls writer absorbed {sz} bytes from {total_len}-byte slices"
                        );
                        buffered_size += sz;
                    }
                    Err(e) => match e.kind() {
                        ErrorKind::WouldBlock => {}
                        ErrorKind::ConnectionReset
                        | ErrorKind::ConnectionAborted
                        | ErrorKind::BrokenPipe => {
                            incr!(names::rustls::WRITE_ERROR);
                            is_closed = true;
                            self.peer_reset = true;
                            break;
                        }
                        _ => {
                            error!(
                                "{} could not write data to TLS stream: {:?}",
                                log_socket_context!(self),
                                e
                            );
                            incr!(names::rustls::WRITE_ERROR);
                            is_error = true;
                            break;
                        }
                    },
                }
            }

            // Plaintext was partially absorbed — we cannot re-call write_vectored
            // because the IoSlice pointers have not been advanced. Drain whatever
            // rustls buffered to the socket, then return the partial count so the
            // caller can consume and retry with adjusted slices.
            if buffered_size > 0 && buffered_size < total_len {
                loop {
                    match flush_tls(&mut self.session, &mut self.stream) {
                        Ok(0) => break,
                        Ok(_) => {}
                        Err(e) => match e.kind() {
                            ErrorKind::WouldBlock => {
                                can_write = false;
                                break;
                            }
                            ErrorKind::ConnectionReset
                            | ErrorKind::ConnectionAborted
                            | ErrorKind::BrokenPipe => {
                                incr!(names::rustls::WRITE_ERROR);
                                is_closed = true;
                                self.peer_reset = true;
                                break;
                            }
                            _ => {
                                error!(
                                    "{} could not write TLS stream to socket: {:?}",
                                    log_socket_context!(self),
                                    e
                                );
                                incr!(names::rustls::WRITE_ERROR);
                                is_error = true;
                                break;
                            }
                        },
                    }
                }
                break;
            }

            loop {
                match flush_tls(&mut self.session, &mut self.stream) {
                    Ok(0) => {
                        break;
                    }
                    Ok(_sz) => {}
                    Err(e) => match e.kind() {
                        ErrorKind::WouldBlock => {
                            can_write = false;
                            break;
                        }
                        ErrorKind::ConnectionReset
                        | ErrorKind::ConnectionAborted
                        | ErrorKind::BrokenPipe => {
                            incr!(names::rustls::WRITE_ERROR);
                            is_closed = true;
                            self.peer_reset = true;
                            break;
                        }
                        _ => {
                            error!(
                                "{} could not write TLS stream to socket: {:?}",
                                log_socket_context!(self),
                                e
                            );
                            incr!(names::rustls::WRITE_ERROR);
                            is_error = true;
                            break;
                        }
                    },
                }
            }
        }

        if !is_error && !is_closed && can_write && self.session.wants_write() {
            loop {
                match flush_tls(&mut self.session, &mut self.stream) {
                    Ok(0) => break,
                    Ok(_) => {}
                    Err(e) => match e.kind() {
                        ErrorKind::WouldBlock => {
                            can_write = false;
                            break;
                        }
                        ErrorKind::ConnectionReset
                        | ErrorKind::ConnectionAborted
                        | ErrorKind::BrokenPipe => {
                            incr!(names::rustls::WRITE_ERROR);
                            is_closed = true;
                            self.peer_reset = true;
                            break;
                        }
                        _ => {
                            error!(
                                "{} could not flush TLS stream to socket: {:?}",
                                log_socket_context!(self),
                                e
                            );
                            incr!(names::rustls::WRITE_ERROR);
                            is_error = true;
                            break;
                        }
                    },
                }
            }
        }

        // Post-condition: report no more than the summed slice length, and keep
        // Error/Closed mutually exclusive — must stay structurally symmetric with
        // `socket_write` (divergence here is the 4.5 MB truncation-class bug).
        debug_assert!(
            buffered_size <= total_len,
            "rustls socket_write_vectored reported {buffered_size} bytes for {total_len}-byte slices"
        );
        debug_assert!(
            !(is_error && is_closed),
            "rustls socket_write_vectored cannot be both Error and Closed"
        );
        if is_error {
            // Same as `socket_write`: an `Error` always marks the channel dead.
            self.peer_reset = true;
            (buffered_size, SocketResult::Error)
        } else if is_closed {
            (buffered_size, SocketResult::Closed)
        } else if !can_write {
            (buffered_size, SocketResult::WouldBlock)
        } else {
            (buffered_size, SocketResult::Continue)
        }
    }

    fn socket_close(&mut self) {
        self.session.send_close_notify();
    }

    /// Queue `buf` and the `close_notify` behind it, then flush both with one
    /// `write_tls`, which hands every pending record to one `write_vectored`
    /// (`rustls-0.23.45/src/vecbuf.rs`, `ChunkVecBuffer::write_to`).
    ///
    /// The plaintext must be queued first: rustls encrypts it into the send
    /// buffer at once (the handshake is over on every connection the mux
    /// drives), and the alert queued after it is the next record on the wire,
    /// so the peer reads the bytes and then the close. The other order would
    /// end the peer's reading before the bytes.
    ///
    /// When rustls does not take all of `buf` (its send buffer is full) the
    /// alert is not queued, as the trait requires, and the caller's next call
    /// carries the rest. A write error falls back to [`Self::socket_write`],
    /// which reports it with the socket context.
    fn socket_write_then_close(&mut self, buf: &[u8]) -> (usize, SocketResult) {
        if self.peer_reset {
            return (0, SocketResult::Closed);
        }
        match self.session.writer().write(buf) {
            Ok(absorbed) => {
                if absorbed == buf.len() {
                    self.session.send_close_notify();
                }
                let (_flushed, status) = self.socket_write(&[]);
                (absorbed, status)
            }
            Err(_) => self.socket_write(buf),
        }
    }

    fn socket_wants_write(&self) -> bool {
        // Only a dead channel (a RST, or a failed socket write) stops us
        // wanting to write — a peer FIN still allows flushing TLS plaintext
        // buffered in rustls (half-close).
        !self.peer_reset && self.session.wants_write()
    }

    fn socket_ref(&self) -> &TcpStream {
        &self.stream
    }

    fn socket_mut(&mut self) -> &mut TcpStream {
        &mut self.stream
    }

    /// No production route reaches the fallback any more. The expect-proxy
    /// route gets there only through `upgrade_expect`, which returns `None`
    /// unless both PROXY addresses parse, so `peer_address` is `Some` by the
    /// time a handshake exists; the direct route seeds it in
    /// `HttpsSession::new` (`https.rs`) with the address `accept(2)` returned
    /// with the socket, which cannot fail the way `getpeername(2)` can. The
    /// arm stays as defence in depth: a handler built without a peer would
    /// otherwise blank the `peer=` slot of a healthy connection. Do not
    /// delete it — `https::tests::front_rustls_peer_addr_falls_back_to_the_live_lookup`
    /// builds such a handler by hand and fails without it.
    fn peer_addr(&self) -> Option<SocketAddr> {
        self.configured_peer
            .or_else(|| self.stream.peer_addr().ok())
    }

    fn protocol(&self) -> TransportProtocol {
        self.session
            .protocol_version()
            .map(|version| match version {
                ProtocolVersion::SSLv2 => TransportProtocol::Ssl2,
                ProtocolVersion::SSLv3 => TransportProtocol::Ssl3,
                ProtocolVersion::TLSv1_0 => TransportProtocol::Tls1_0,
                ProtocolVersion::TLSv1_1 => TransportProtocol::Tls1_1,
                ProtocolVersion::TLSv1_2 => TransportProtocol::Tls1_2,
                ProtocolVersion::TLSv1_3 => TransportProtocol::Tls1_3,
                _ => TransportProtocol::Tls1_3,
            })
            .unwrap_or(TransportProtocol::Tcp)
    }

    fn read_error(&self) {
        incr!(names::rustls::READ_ERROR);
    }

    fn write_error(&self) {
        incr!(names::rustls::WRITE_ERROR);
    }

    fn session_ulid(&self) -> Option<Ulid> {
        Some(self.session_ulid)
    }
}

/// Bind `sock`, the listening socket for `addr`, to the network `interface`
/// (`SO_BINDTODEVICE`), before `bind(2)`: the listener then accepts only what
/// arrives through that interface, whatever address it holds, which is what
/// lets a listener follow an interface whose address is not known ahead (a
/// WireGuard `wg0`). `None` leaves the socket unbound, as before interfaces
/// existed.
///
/// The option is set before `bind(2)` so that two listeners on the same
/// address, bound to different interfaces, can both bind under
/// `SO_REUSEADDR`/`SO_REUSEPORT`. It survives the SCM hand-off of an upgrade:
/// it is a property of the socket, not of the process.
fn bind_to_device(
    sock: &Socket,
    addr: SocketAddr,
    interface: Option<&str>,
) -> Result<(), ServerBindError> {
    let Some(interface) = interface else {
        return Ok(());
    };
    #[cfg(target_os = "linux")]
    {
        sock.bind_device(Some(interface.as_bytes()))
            .map_err(|error| ServerBindError::BindToDevice {
                address: addr,
                interface: interface.to_owned(),
                error,
            })?;
        debug_assert!(
            sock.device().map_or(true, |device| device.as_deref()
                == Some(interface.as_bytes())),
            "a socket bound to an interface must report that interface"
        );
        Ok(())
    }
    #[cfg(not(target_os = "linux"))]
    {
        let _ = sock;
        Err(ServerBindError::BindToDeviceUnsupported {
            address: addr,
            interface: interface.to_owned(),
        })
    }
}

pub fn server_bind(
    addr: SocketAddr,
    interface: Option<&str>,
) -> Result<TcpListener, ServerBindError> {
    let sock = Socket::new(Domain::for_address(addr), Type::STREAM, Some(Protocol::TCP))
        .map_err(ServerBindError::SocketCreationError)?;

    bind_to_device(&sock, addr, interface)?;

    // set so_reuseaddr, but only on unix (mirrors what libstd does)
    if cfg!(unix) {
        sock.set_reuse_address(true)
            .map_err(ServerBindError::SetReuseAddress)?;
    }

    sock.set_reuse_port(true)
        .map_err(ServerBindError::SetReusePort)?;

    sock.bind(&addr.into())
        .map_err(ServerBindError::BindError)?;

    sock.set_nonblocking(true)
        .map_err(ServerBindError::SetNonBlocking)?;

    // listen
    // FIXME: make the backlog configurable?
    sock.listen(1024).map_err(ServerBindError::Listen)?;

    // Post-conditions (invariant violations only — every fallible syscall above
    // already returns an error; these `debug_assert!`s catch a flag we *set*
    // silently not sticking, which would be our own logic bug, not a syscall
    // failure on network input). The getters return `io::Result`; we only
    // assert when the kernel answers, degrading to a no-op on the rare query
    // failure so we never panic on a dying fd.
    if let Ok(nonblocking) = sock.nonblocking() {
        debug_assert!(
            nonblocking,
            "server_bind must return a non-blocking socket (the worker event loop is edge-triggered)"
        );
    }
    // `SO_REUSEPORT` is set on every platform; assert it stuck so a SCM hand-off
    // across a hot-upgrade can re-bind the same address.
    #[cfg(unix)]
    if let Ok(reuse_port) = sock.reuse_port() {
        debug_assert!(
            reuse_port,
            "server_bind must set SO_REUSEPORT so the listener survives a hot-upgrade re-bind"
        );
    }
    // `SO_REUSEADDR` is unix-only here (mirrors libstd).
    #[cfg(unix)]
    if let Ok(reuse_address) = sock.reuse_address() {
        debug_assert!(
            reuse_address,
            "server_bind must set SO_REUSEADDR on unix (mirrors libstd)"
        );
    }
    // A bound STREAM socket carries a local address in the requested family.
    if let Ok(local) = sock.local_addr() {
        debug_assert_eq!(
            local.is_ipv4(),
            addr.is_ipv4(),
            "bound socket family must match the requested address family"
        );
        debug_assert_eq!(
            local.is_ipv6(),
            addr.is_ipv6(),
            "bound socket family must match the requested address family"
        );
    }

    Ok(TcpListener::from_std(sock.into()))
}

/// Bind a non-blocking UDP listener socket on `addr`.
///
/// Mirrors [`server_bind`] but for DGRAM: `SO_REUSEADDR` (unix) + `SO_REUSEPORT`
/// so the socket can be SCM-passed and re-bound across a hot-upgrade, then
/// `bind` + non-blocking. Unlike TCP there is **no `listen()`** — a UDP socket
/// receives datagrams directly. The returned `mio::net::UdpSocket` is the one
/// listener socket the UDP datapath demuxes many flows over (one-socket-many-
/// flows; per-flow return sockets are created by [`udp_connect`]).
pub fn udp_bind(addr: SocketAddr, interface: Option<&str>) -> Result<UdpSocket, ServerBindError> {
    let sock = Socket::new(Domain::for_address(addr), Type::DGRAM, Some(Protocol::UDP))
        .map_err(ServerBindError::SocketCreationError)?;

    bind_to_device(&sock, addr, interface)?;

    // set so_reuseaddr, but only on unix (mirrors what libstd does)
    if cfg!(unix) {
        sock.set_reuse_address(true)
            .map_err(ServerBindError::SetReuseAddress)?;
    }

    sock.set_reuse_port(true)
        .map_err(ServerBindError::SetReusePort)?;

    sock.bind(&addr.into())
        .map_err(ServerBindError::BindError)?;

    sock.set_nonblocking(true)
        .map_err(ServerBindError::SetNonBlocking)?;

    // No `listen()` for DGRAM sockets.

    // Post-conditions — same rationale as `server_bind`: assert the flags we set
    // stuck (logic bug if not), degrading to a no-op when the kernel refuses the
    // query so a dying fd never panics. There is deliberately no `listen()`
    // check here: DGRAM sockets are never listened on.
    if let Ok(nonblocking) = sock.nonblocking() {
        debug_assert!(
            nonblocking,
            "udp_bind must return a non-blocking socket (the worker event loop is edge-triggered)"
        );
    }
    #[cfg(unix)]
    if let Ok(reuse_port) = sock.reuse_port() {
        debug_assert!(
            reuse_port,
            "udp_bind must set SO_REUSEPORT so the listener survives a hot-upgrade re-bind"
        );
    }
    #[cfg(unix)]
    if let Ok(reuse_address) = sock.reuse_address() {
        debug_assert!(
            reuse_address,
            "udp_bind must set SO_REUSEADDR on unix (mirrors libstd / server_bind)"
        );
    }
    if let Ok(local) = sock.local_addr() {
        debug_assert_eq!(
            local.is_ipv4(),
            addr.is_ipv4(),
            "bound UDP socket family must match the requested address family"
        );
        debug_assert_eq!(
            local.is_ipv6(),
            addr.is_ipv6(),
            "bound UDP socket family must match the requested address family"
        );
    }

    Ok(UdpSocket::from_std(sock.into()))
}

/// Create a non-blocking **connected** per-flow upstream UDP socket toward
/// `backend`.
///
/// The socket is bound to an ephemeral local port (family matched to the
/// backend) and `connect`-ed to the backend address. A connected UDP socket
/// "only receives from the connected address" (`connect(2)`), so its fd is the
/// symmetric-NAT return-demux key for one flow: the shell registers
/// `upstream_token -> FlowId` and feeds anything that arrives on it back into
/// the manager as a `BackendDatagram`. `send` (not `send_to`) is then used for
/// the forward path. Errors (`EMFILE`/`ENFILE`/connect refusal) bubble up so
/// the caller can shed the flow rather than panic.
///
/// On Linux the socket is created non-blocking and close-on-exec in the one
/// `socket(2)` call (`SOCK_NONBLOCK | SOCK_CLOEXEC`; `Socket::new` adds the
/// latter), so a new flow costs `socket`, `bind`, `connect` and no `fcntl(2)`.
/// Other platforms keep the `set_nonblocking` round trip. The post-condition
/// queries (`fcntl(F_GETFL)`, `getsockname(2)`, `getpeername(2)`) only feed
/// `debug_assert!`s, so they are compiled out of release builds.
pub fn udp_connect(backend: SocketAddr) -> Result<UdpSocket, ServerBindError> {
    let unspecified: SocketAddr = match backend {
        SocketAddr::V4(_) => (std::net::Ipv4Addr::UNSPECIFIED, 0).into(),
        SocketAddr::V6(_) => (std::net::Ipv6Addr::UNSPECIFIED, 0).into(),
    };
    // The ephemeral bind address must be in the backend's family with port 0,
    // or the subsequent `connect` would mix families / pin a wrong local port.
    debug_assert_eq!(
        unspecified.is_ipv4(),
        backend.is_ipv4(),
        "ephemeral bind family must match the backend family"
    );
    debug_assert_eq!(
        unspecified.port(),
        0,
        "ephemeral bind must use port 0 so the kernel picks the source port"
    );
    // Linux takes `O_NONBLOCK` at creation, which saves the `F_GETFL`/`F_SETFL`
    // pair of `set_nonblocking` on every new flow.
    #[cfg(target_os = "linux")]
    let ty = Type::DGRAM.nonblocking();
    #[cfg(not(target_os = "linux"))]
    let ty = Type::DGRAM;
    let sock = Socket::new(Domain::for_address(backend), ty, Some(Protocol::UDP))
        .map_err(ServerBindError::SocketCreationError)?;

    sock.bind(&unspecified.into())
        .map_err(ServerBindError::BindError)?;
    #[cfg(not(target_os = "linux"))]
    sock.set_nonblocking(true)
        .map_err(ServerBindError::SetNonBlocking)?;
    // `connect` on a DGRAM socket pins the return 4-tuple; a non-blocking
    // connect on UDP completes immediately (no handshake).
    sock.connect(&backend.into())
        .map_err(ServerBindError::BindError)?;

    // Post-conditions — assert the flag/connect state stuck (logic bug if not),
    // degrading to a no-op when the kernel refuses the query so a dying fd never
    // panics on this network-facing path. Each query is a syscall that only
    // feeds a `debug_assert!`, so release builds skip them on this per-flow path.
    #[cfg(debug_assertions)]
    {
        if let Ok(nonblocking) = sock.nonblocking() {
            debug_assert!(
                nonblocking,
                "udp_connect must return a non-blocking socket (the worker event loop is edge-triggered)"
            );
        }
        // The connected return socket's local addr family must match the backend,
        // and the kernel must have assigned a concrete source port (no longer 0).
        if let Ok(local) = sock.local_addr() {
            debug_assert_eq!(
                local.is_ipv4(),
                backend.is_ipv4(),
                "connected UDP socket family must match the backend family"
            );
            if let Some(local) = local.as_socket() {
                debug_assert_ne!(
                    local.port(),
                    0,
                    "connect must bind a concrete ephemeral source port (the return-demux key)"
                );
            }
        }
        // `connect` pinned the peer 4-tuple — `getpeername(2)` must echo the backend.
        if let Ok(peer) = sock.peer_addr()
            && let Some(peer) = peer.as_socket()
        {
            debug_assert_eq!(
                peer, backend,
                "connect must pin the peer to the requested backend (symmetric-NAT return-demux key)"
            );
        }
    }

    Ok(UdpSocket::from_std(sock.into()))
}

/// Socket statistics
pub mod stats {
    use std::{os::fd::AsRawFd, time::Duration};

    use internal::{OPT_LEVEL, OPT_NAME, TcpInfo};

    /// Point-in-time snapshot of kernel TCP bookkeeping for a socket. Populated
    /// from a single `getsockopt(TCP_INFO)` syscall so callers that want both
    /// the smoothed RTT and the FSM state don't pay for two trips into the
    /// kernel. Field set is deliberately narrow — extend with more `tcp_info`
    /// members if the log prefix grows.
    #[derive(Clone, Debug)]
    pub struct TcpSnapshot {
        pub rtt: Duration,
        pub state: &'static str,
    }

    /// Round trip time for a TCP socket. Kept for existing metric callers;
    /// log-prefix callers should prefer [`socket_snapshot`] which returns the
    /// RTT **and** the TCP FSM state from a single syscall.
    pub fn socket_rtt<A: AsRawFd>(socket: &A) -> Option<Duration> {
        socket_info(socket.as_raw_fd()).map(|info| Duration::from_micros(info.rtt() as u64))
    }

    /// Smoothed RTT + human-readable TCP state (`"ESTABLISHED"`, `"SYN_SENT"`,
    /// `"CLOSE_WAIT"`, …) pulled from a single `getsockopt(TCP_INFO)` call.
    /// Returns `None` when the kernel refuses the call — e.g. the socket has
    /// been closed, or the FSM is in a state where `TCP_INFO` is not usable.
    /// Safe on dying/refused sockets: the inner syscall's `status != 0`
    /// branch is the only failure mode and it degrades to `None`.
    pub fn socket_snapshot<A: AsRawFd>(socket: &A) -> Option<TcpSnapshot> {
        socket_info(socket.as_raw_fd()).map(|info| TcpSnapshot {
            rtt: Duration::from_micros(info.rtt() as u64),
            state: info.state(),
        })
    }

    #[cfg(unix)]
    pub fn socket_info(fd: libc::c_int) -> Option<TcpInfo> {
        // SAFETY: `TcpInfo` is a C POD whose every byte pattern is a legal
        // representation; zero-init satisfies `assume_init`'s invariant
        // (and `std::mem::zeroed` is the canonical idiom for that).
        let mut tcp_info: TcpInfo = unsafe { std::mem::zeroed() };
        let struct_len = std::mem::size_of::<TcpInfo>() as libc::socklen_t;
        let mut len = struct_len;
        // SAFETY: `tcp_info` and `len` are fully initialised above; libc
        // reads only `len` bytes through the pointer and writes back the
        // resulting length. We check the return value (`status != 0`) to
        // distinguish success from validation failure.
        let status = unsafe {
            libc::getsockopt(
                fd,
                OPT_LEVEL,
                OPT_NAME,
                &mut tcp_info as *mut _ as *mut _,
                &mut len,
            )
        };
        if status != 0 {
            None
        } else {
            // The kernel writes back the number of bytes it populated. It must
            // never claim to have written more than the buffer we handed it —
            // that would mean it overran `tcp_info`, an out-of-bounds write we
            // could not have detected by the return code alone.
            debug_assert!(
                len <= struct_len,
                "getsockopt(TCP_INFO) wrote back len {len} > struct size {struct_len} (buffer overrun)"
            );
            Some(tcp_info)
        }
    }
    #[cfg(not(unix))]
    pub fn socketinfo(fd: libc::c_int) -> Option<TcpInfo> {
        None
    }

    #[cfg(unix)]
    #[cfg(not(any(target_os = "macos", target_os = "ios")))]
    mod internal {
        #[cfg(target_os = "linux")]
        pub const OPT_LEVEL: libc::c_int = libc::SOL_TCP;

        #[cfg(any(
            target_os = "freebsd",
            target_os = "dragonfly",
            target_os = "openbsd",
            target_os = "netbsd"
        ))]
        pub const OPT_LEVEL: libc::c_int = libc::IPPROTO_TCP;

        pub const OPT_NAME: libc::c_int = libc::TCP_INFO;

        #[derive(Clone, Debug)]
        #[repr(C)]
        pub struct TcpInfo {
            // State
            tcpi_state: u8,
            tcpi_ca_state: u8,
            tcpi_retransmits: u8,
            tcpi_probes: u8,
            tcpi_backoff: u8,
            tcpi_options: u8,
            tcpi_snd_rcv_wscale: u8, // 4bits|4bits

            tcpi_rto: u32,
            tcpi_ato: u32,
            tcpi_snd_mss: u32,
            tcpi_rcv_mss: u32,

            tcpi_unacked: u32,
            tcpi_sacked: u32,
            tcpi_lost: u32,
            tcpi_retrans: u32,
            tcpi_fackets: u32,

            // Times
            tcpi_last_data_sent: u32,
            tcpi_last_ack_sent: u32, // Not remembered
            tcpi_last_data_recv: u32,
            tcpi_last_ack_recv: u32,

            // Metrics
            tcpi_pmtu: u32,
            tcpi_rcv_ssthresh: u32,
            tcpi_rtt: u32,
            tcpi_rttvar: u32,
            tcpi_snd_ssthresh: u32,
            tcpi_snd_cwnd: u32,
            tcpi_advmss: u32,
            tcpi_reordering: u32,
        }
        impl TcpInfo {
            pub fn rtt(&self) -> u32 {
                self.tcpi_rtt
            }

            /// Human-readable Linux TCP FSM state. Values follow
            /// `include/net/tcp_states.h` (`TCP_ESTABLISHED = 1`,
            /// `TCP_SYN_SENT = 2`, …). Anything unexpected falls back to
            /// `"UNKNOWN"` rather than panicking — the log prefix is a
            /// best-effort diagnostic and must not add failure modes.
            pub fn state(&self) -> &'static str {
                match self.tcpi_state {
                    1 => "ESTABLISHED",
                    2 => "SYN_SENT",
                    3 => "SYN_RECV",
                    4 => "FIN_WAIT1",
                    5 => "FIN_WAIT2",
                    6 => "TIME_WAIT",
                    7 => "CLOSE",
                    8 => "CLOSE_WAIT",
                    9 => "LAST_ACK",
                    10 => "LISTEN",
                    11 => "CLOSING",
                    12 => "NEW_SYN_RECV",
                    _ => "UNKNOWN",
                }
            }
        }
    }

    #[cfg(unix)]
    #[cfg(any(target_os = "macos", target_os = "ios"))]
    mod internal {
        pub const OPT_LEVEL: libc::c_int = libc::IPPROTO_TCP;
        pub const OPT_NAME: libc::c_int = 0x106;

        #[derive(Clone, Debug)]
        #[repr(C)]
        pub struct TcpInfo {
            tcpi_state: u8,
            tcpi_snd_wscale: u8,
            tcpi_rcv_wscale: u8,
            __pad1: u8,
            tcpi_options: u32,
            tcpi_flags: u32,
            tcpi_rto: u32,
            tcpi_maxseg: u32,
            tcpi_snd_ssthresh: u32,
            tcpi_snd_cwnd: u32,
            tcpi_snd_wnd: u32,
            tcpi_snd_sbbytes: u32,
            tcpi_rcv_wnd: u32,
            tcpi_rttcur: u32,
            tcpi_srtt: u32,
            tcpi_rttvar: u32,
            tcpi_tfo: u32,
            tcpi_txpackets: u64,
            tcpi_txbytes: u64,
            tcpi_txretransmitbytes: u64,
            tcpi_rxpackets: u64,
            tcpi_rxbytes: u64,
            tcpi_rxoutoforderbytes: u64,
            tcpi_txretransmitpackets: u64,
        }
        impl TcpInfo {
            pub fn rtt(&self) -> u32 {
                // tcpi_srtt is in milliseconds not microseconds
                self.tcpi_srtt * 1000
            }

            /// Human-readable Darwin TCP FSM state. Values follow
            /// `netinet/tcp_fsm.h` (`TCPS_CLOSED = 0`, `TCPS_LISTEN = 1`,
            /// `TCPS_SYN_SENT = 2`, …). Differs from Linux numbering —
            /// macOS counts from 0, Linux from 1.
            pub fn state(&self) -> &'static str {
                match self.tcpi_state {
                    0 => "CLOSED",
                    1 => "LISTEN",
                    2 => "SYN_SENT",
                    3 => "SYN_RECEIVED",
                    4 => "ESTABLISHED",
                    5 => "CLOSE_WAIT",
                    6 => "FIN_WAIT_1",
                    7 => "CLOSING",
                    8 => "LAST_ACK",
                    9 => "FIN_WAIT_2",
                    10 => "TIME_WAIT",
                    _ => "UNKNOWN",
                }
            }
        }
    }

    #[cfg(not(unix))]
    #[derive(Clone, Debug)]
    struct TcpInfo {}

    #[test]
    #[serial_test::serial]
    fn test_rtt() {
        let sock = std::net::TcpStream::connect("google.com:80").unwrap();
        let fd = sock.as_raw_fd();
        let info = socket_info(fd);
        assert!(info.is_some());
        println!("{info:#?}");
        println!(
            "rtt: {}",
            sozu_command::logging::LogDuration(socket_rtt(&sock))
        );
    }
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;

    /// Binding a listener to an interface needs `CAP_NET_RAW` on kernels
    /// before 5.7; an environment without it cannot run the positive test.
    #[cfg(target_os = "linux")]
    fn lacks_bind_to_device_permission(error: &ServerBindError) -> bool {
        matches!(
            error,
            ServerBindError::BindToDevice { error, .. }
                if error.kind() == std::io::ErrorKind::PermissionDenied
        )
    }

    /// `server_bind` and `udp_bind` bind a listener with an `interface` to
    /// that interface (`SO_BINDTODEVICE`, read back with `getsockopt`), and a
    /// listener without one to none — today's behaviour.
    #[cfg(target_os = "linux")]
    #[test]
    fn bind_sets_so_bindtodevice_to_the_listener_interface() {
        use socket2::SockRef;

        let addr: SocketAddr = "127.0.0.1:0".parse().unwrap();

        let tcp = match server_bind(addr, Some("lo")) {
            Ok(listener) => listener,
            Err(error) if lacks_bind_to_device_permission(&error) => {
                eprintln!(
                    "SKIPPED bind_sets_so_bindtodevice_to_the_listener_interface: this \
                     environment may not bind a socket to an interface (needs CAP_NET_RAW \
                     before Linux 5.7): {error}"
                );
                return;
            }
            Err(error) => panic!("binding a TCP listener to lo: {error}"),
        };
        assert_eq!(
            SockRef::from(&tcp)
                .device()
                .expect("getsockopt(SO_BINDTODEVICE)"),
            Some(b"lo".to_vec()),
            "the TCP listener must be bound to lo"
        );

        let udp = udp_bind(addr, Some("lo")).expect("binding a UDP listener to lo");
        assert_eq!(
            SockRef::from(&udp)
                .device()
                .expect("getsockopt(SO_BINDTODEVICE)"),
            Some(b"lo".to_vec()),
            "the UDP listener must be bound to lo"
        );

        let plain = server_bind(addr, None).expect("binding a TCP listener");
        assert_eq!(
            SockRef::from(&plain)
                .device()
                .expect("getsockopt(SO_BINDTODEVICE)"),
            None,
            "a listener without interface stays bound to no interface"
        );
    }

    /// An interface that does not exist fails the bind with an explicit error
    /// naming the listener address and the interface, and pointing at the two
    /// usual causes: the interface must exist, and pre-5.7 kernels require
    /// `CAP_NET_RAW`.
    #[cfg(target_os = "linux")]
    #[test]
    fn bind_to_a_missing_interface_names_the_interface_and_the_capability() {
        let addr: SocketAddr = "127.0.0.1:0".parse().unwrap();
        for result in [
            server_bind(addr, Some("sozu-nosuch0")).map(|_| ()),
            udp_bind(addr, Some("sozu-nosuch0")).map(|_| ()),
        ] {
            let error = result.expect_err("binding to a missing interface must fail");
            assert!(
                matches!(&error, ServerBindError::BindToDevice { interface, .. } if interface == "sozu-nosuch0"),
                "{error:?}"
            );
            let message = error.to_string();
            for needle in [
                "127.0.0.1:0",
                "'sozu-nosuch0'",
                "SO_BINDTODEVICE",
                "must exist",
                "CAP_NET_RAW",
                "5.7",
            ] {
                assert!(
                    message.contains(needle),
                    "{needle:?} missing from: {message}"
                );
            }
        }
    }

    /// Address a handler caches. Deliberately non-loopback so it can never
    /// collide with the ephemeral address a live `getpeername(2)` reports for
    /// the test connection.
    const CACHED_PEER: &str = "10.0.0.42:12345";

    /// A live, established loopback connection plus the address
    /// `getpeername(2)` reports for it. The listener is returned so the
    /// connection stays up for the whole test: the property under test is that
    /// the snapshot wins even while the live lookup is perfectly healthy, and a
    /// half-dead socket would weaken that rather than strengthen it.
    ///
    /// Using a genuinely connected socket instead of an aborted `connect()` is
    /// also what makes these tests deterministic: whether a nonblocking connect
    /// to a closed port has already collected its RST — and therefore whether
    /// `getpeername(2)` has started answering `ENOTCONN` — is a race against the
    /// kernel, not something a test may assert on.
    fn connected_loopback_stream() -> (std::net::TcpListener, TcpStream, SocketAddr) {
        let listener = std::net::TcpListener::bind("127.0.0.1:0")
            .expect("test listener must bind to a loopback port");
        let live_peer = listener
            .local_addr()
            .expect("test listener must report its local address");
        let stream =
            std::net::TcpStream::connect(live_peer).expect("loopback connect must complete");
        stream
            .set_nonblocking(true)
            .expect("mio requires a nonblocking stream");
        (listener, TcpStream::from_std(stream), live_peer)
    }

    /// The cache is the whole point of the snapshot: on a PROXY-protocol
    /// frontend it holds the advertised client while the socket's own peer is
    /// the load balancer, and on a backend socket it holds the configured
    /// backend while `getpeername(2)` answers `ENOTCONN` after a failed
    /// `connect()`. A handler that consults the kernel first has neither.
    ///
    /// To SEE THIS RED: in `impl SocketHandler for SessionTcpStream`, replace
    /// the body of `peer_addr` with `self.stream.peer_addr().ok()`. The cached
    /// address disappears and the loopback address takes its place.
    #[test]
    fn session_tcp_stream_peer_addr_prefers_the_cached_address() {
        let (_listener, stream, live_peer) = connected_loopback_stream();
        let cached: SocketAddr = CACHED_PEER
            .parse()
            .expect("the cached peer literal must parse");
        let handler = SessionTcpStream::new(stream, Ulid::generate(), Some(cached));

        // Premise: the live lookup is healthy and disagrees with the cache, so
        // the assertion below cannot pass for the wrong reason.
        assert_eq!(
            handler.socket_ref().peer_addr().ok(),
            Some(live_peer),
            "the test socket must be genuinely connected, so a live lookup succeeds"
        );
        assert_ne!(
            cached, live_peer,
            "the cached and live addresses must differ for this test to discriminate"
        );

        assert_eq!(
            handler.peer_addr(),
            Some(cached),
            "a cached peer address must win over a live getpeername(2)"
        );
    }

    /// The fallback arm. Without it a handler built with no snapshot would
    /// render `peer=None` on every line rather than degrading to the live
    /// lookup that was there before the cache existed.
    ///
    /// To SEE THIS RED: in `impl SocketHandler for SessionTcpStream`, drop the
    /// `.or_else(|| self.stream.peer_addr().ok())` from `peer_addr`. The
    /// handler then answers `None` for a perfectly healthy connection.
    #[test]
    fn session_tcp_stream_peer_addr_falls_back_to_the_live_lookup() {
        let (_listener, stream, live_peer) = connected_loopback_stream();
        let handler = SessionTcpStream::new(stream, Ulid::generate(), None);

        assert_eq!(
            handler.peer_addr(),
            Some(live_peer),
            "with nothing cached, peer_addr must degrade to getpeername(2)"
        );
    }

    /// A bare `TcpStream` carries no session context and therefore nothing to
    /// cache; its `peer_addr` is the live lookup and nothing else. Pinned so
    /// that a later attempt to give `SocketHandler::peer_addr` a default body
    /// has to confront this impl explicitly.
    ///
    /// To SEE THIS RED: in `impl SocketHandler for TcpStream`, replace the body
    /// of `peer_addr` with `None`.
    #[test]
    fn raw_tcp_stream_peer_addr_is_the_live_lookup() {
        let (_listener, stream, live_peer) = connected_loopback_stream();

        assert_eq!(
            SocketHandler::peer_addr(&stream),
            Some(live_peer),
            "the contextless handler must report the kernel's answer"
        );
    }

    /// The `peer=` slot of a `SOCKET` line is the handler's cached address,
    /// the same one `MUX-H2` and `log_socket_module_prefix` render — not a
    /// live `getpeername(2)`.
    ///
    /// Renders through [`log_socket_context!`] itself rather than through
    /// [`SocketHandler::peer_addr`], because the accessor was already correct
    /// for every impl while the macro consulted the kernel behind its back.
    /// The three `peer_addr` tests above therefore all passed while a `SOCKET`
    /// line still named the load balancer on a PROXY-protocol TLS frontend.
    ///
    /// Expanded against [`SessionTcpStream`] and not [`FrontRustls`], which is
    /// the macro's only production caller, because a `FrontRustls` needs a
    /// completed rustls handshake and this module builds no TLS. The macro is
    /// generic over [`SocketHandler`], so what this pins is the slot's
    /// resolution for every impl; that the TLS frontend really renders the
    /// PROXY-advertised client on the wire is pinned end to end by
    /// `e2e::tests::socket_log_context_tests::    /// test_tls_socket_log_peer_is_the_advertised_client`.
    ///
    /// The palette is empty here — `LOGGER_COLORED` defaults to `false` and
    /// this test installs no logger — so the slot is matched as plain text. A
    /// coloured palette would split `peer=` with an escape sequence and redden
    /// the test rather than silently passing it.
    ///
    /// To SEE THIS RED: in [`log_socket_context!`], replace
    /// `peer = crate::socket::SocketHandler::peer_addr($self),` with the
    /// pre-fix `peer = $self.socket_ref().peer_addr().ok(),`.
    #[test]
    fn log_socket_context_renders_the_cached_peer_not_a_live_lookup() {
        let (_listener, stream, live_peer) = connected_loopback_stream();
        let cached: SocketAddr = CACHED_PEER
            .parse()
            .expect("the cached peer literal must parse");
        let handler = SessionTcpStream::new(stream, Ulid::generate(), Some(cached));

        // Premise: the live lookup is healthy and disagrees with the cache, so
        // neither assertion below can pass for the wrong reason.
        assert_eq!(
            handler.socket_ref().peer_addr().ok(),
            Some(live_peer),
            "the test socket must be genuinely connected, so a live lookup succeeds"
        );
        assert_ne!(
            cached, live_peer,
            "the cached and live addresses must differ for this test to discriminate"
        );

        // `&handler`, not `handler`: the macro's `peer` slot is a
        // fully-qualified `SocketHandler::peer_addr` call, so the expansion
        // binds a shared reference. Production call sites pass `self` from a
        // `&mut self` method and reach the same place by reborrow.
        let rendered = log_socket_context!(&handler);

        assert!(
            rendered.contains(&format!("peer=Some({cached})")),
            "the SOCKET peer= slot must render the cached address; rendered: {rendered}"
        );
        assert!(
            !rendered.contains(&format!("peer=Some({live_peer})")),
            "the SOCKET peer= slot must not render a live getpeername(2); rendered: {rendered}"
        );
        // Scope guard: this change touches `peer` and nothing else. `local` is
        // still `getsockname(2)` on the same socket, which is the stream's own
        // address and never the cached peer.
        assert!(
            rendered.contains(&format!(
                "local=Some({})",
                handler
                    .socket_ref()
                    .local_addr()
                    .expect("a connected socket has a local address")
            )),
            "the SOCKET local= slot must stay getsockname(2); rendered: {rendered}"
        );
    }

    /// A socket that was never connected, and therefore one whose
    /// `getpeername(2)` fails with `ENOTCONN` *deterministically*.
    ///
    /// This is the one reliable way to stage a failing live lookup in a test.
    /// The obvious alternative — connect to a closed port and wait for the RST
    /// — is the race `connected_loopback_stream`'s note already rules out:
    /// whether the kernel has collected the RST yet, and so whether
    /// `getpeername(2)` has started refusing, is not something a test may
    /// assert on. An unconnected socket is in the refusing state from birth.
    fn unconnected_stream() -> TcpStream {
        let socket = Socket::new(Domain::IPV4, Type::STREAM, Some(Protocol::TCP))
            .expect("a test socket must be creatable");
        socket
            .set_nonblocking(true)
            .expect("mio requires a nonblocking stream");
        TcpStream::from_std(std::net::TcpStream::from(socket))
    }

    /// The `peer=` slot survives a live lookup that FAILS, which is the arm an
    /// operator meets during an incident: `getpeername(2)` answers `ENOTCONN`
    /// once the peer has reset, so the pre-fix macro rendered `peer=None` on
    /// exactly the error lines being read at the time.
    ///
    /// Staged with a never-connected socket rather than a reset one, because
    /// only the former refuses deterministically — see
    /// [`unconnected_stream`]. A peer reset is one way to reach that state;
    /// what this pins is the rendering once the socket is in it, which is the
    /// part the macro owns.
    ///
    /// The companion
    /// [`log_socket_context_renders_the_cached_peer_not_a_live_lookup`] covers
    /// the other half, where the live lookup SUCCEEDS and disagrees. The two
    /// are separate tests because a single one cannot be red for both reasons,
    /// and the PROXY-protocol defect lives in the succeeding half.
    ///
    /// To SEE THIS RED: in [`log_socket_context!`], replace
    /// `peer = crate::socket::SocketHandler::peer_addr($self),` with the
    /// pre-fix `peer = $self.socket_ref().peer_addr().ok(),`. The slot
    /// collapses to `peer=None`.
    #[test]
    fn log_socket_context_renders_the_cached_peer_when_the_live_lookup_fails() {
        let stream = unconnected_stream();
        let cached: SocketAddr = CACHED_PEER
            .parse()
            .expect("the cached peer literal must parse");

        // Premise: the live lookup really is refusing, so the assertion below
        // cannot pass for the wrong reason.
        assert_eq!(
            stream.peer_addr().ok(),
            None,
            "an unconnected socket must refuse getpeername(2)"
        );

        let handler = SessionTcpStream::new(stream, Ulid::generate(), Some(cached));
        let rendered = log_socket_context!(&handler);

        assert!(
            rendered.contains(&format!("peer=Some({cached})")),
            "the SOCKET peer= slot must survive a failed live lookup; rendered: {rendered}"
        );
        assert!(
            !rendered.contains("peer=None"),
            "the SOCKET peer= slot must not collapse to None while a cache exists; \
             rendered: {rendered}"
        );
    }

    /// Counts, with a seccomp user-notification filter, the `fcntl(2)`,
    /// `getsockname(2)` and `getpeername(2)` calls one `udp_connect` makes,
    /// then checks the returned socket's flags and addresses.
    ///
    /// The filter is installed on a dedicated thread only (no `TSYNC`), so the
    /// rest of the test binary is untouched. Every matching call is reported to
    /// this thread, counted, and let through unchanged
    /// (`SECCOMP_USER_NOTIF_FLAG_CONTINUE`), so `udp_connect` sees the real
    /// kernel answers. Release builds must make none of the three calls: the
    /// socket is born non-blocking and the post-condition queries only feed
    /// `debug_assert!`s. Debug builds keep exactly those queries: one
    /// `F_GETFL`, one `getsockname`, one `getpeername`.
    #[cfg(all(target_os = "linux", target_pointer_width = "64"))]
    #[test]
    fn udp_connect_is_born_nonblocking_and_skips_release_queries() {
        use std::os::fd::{AsRawFd as _, FromRawFd as _, OwnedFd};

        const WATCHED: [libc::c_long; 3] = [
            libc::SYS_fcntl,
            libc::SYS_getsockname,
            libc::SYS_getpeername,
        ];

        let backend_sock =
            std::net::UdpSocket::bind("127.0.0.1:0").expect("the test backend must bind");
        let backend = backend_sock
            .local_addr()
            .expect("the test backend must report its address");

        let (listener_tx, listener_rx) = std::sync::mpsc::channel::<OwnedFd>();
        let worker = std::thread::spawn(move || {
            let stmt = |code: u32, k: u32| libc::sock_filter {
                code: code as u16,
                jt: 0,
                jf: 0,
                k,
            };
            let jeq = |k: libc::c_long, jt: u8| libc::sock_filter {
                code: (libc::BPF_JMP | libc::BPF_JEQ | libc::BPF_K) as u16,
                jt,
                jf: 0,
                k: k as u32,
            };
            // `seccomp_data.nr` is the first field; any watched number jumps
            // to the USER_NOTIF return, everything else is allowed.
            let mut program = [
                stmt(libc::BPF_LD | libc::BPF_W | libc::BPF_ABS, 0),
                jeq(WATCHED[0], 3),
                jeq(WATCHED[1], 2),
                jeq(WATCHED[2], 1),
                stmt(libc::BPF_RET | libc::BPF_K, libc::SECCOMP_RET_ALLOW),
                stmt(libc::BPF_RET | libc::BPF_K, libc::SECCOMP_RET_USER_NOTIF),
            ];
            let fprog = libc::sock_fprog {
                len: program.len() as u16,
                filter: program.as_mut_ptr(),
            };
            // SAFETY: plain syscalls on this thread; `fprog` outlives them.
            let listener = unsafe {
                assert_eq!(
                    libc::prctl(libc::PR_SET_NO_NEW_PRIVS, 1, 0, 0, 0),
                    0,
                    "PR_SET_NO_NEW_PRIVS: {}",
                    std::io::Error::last_os_error()
                );
                libc::syscall(
                    libc::SYS_seccomp,
                    libc::SECCOMP_SET_MODE_FILTER,
                    libc::SECCOMP_FILTER_FLAG_NEW_LISTENER,
                    &fprog as *const libc::sock_fprog,
                )
            };
            assert!(
                listener >= 0,
                "installing the seccomp filter failed: {}",
                std::io::Error::last_os_error()
            );
            // SAFETY: the kernel just returned this descriptor to us.
            let listener = unsafe { OwnedFd::from_raw_fd(listener as libc::c_int) };
            listener_tx
                .send(listener)
                .expect("the counting thread must receive the listener");
            udp_connect(backend)
        });

        let listener = listener_rx
            .recv()
            .expect("the worker must hand over the seccomp listener");
        let mut counts = [0usize; 3];
        loop {
            let mut pollfd = libc::pollfd {
                fd: listener.as_raw_fd(),
                events: libc::POLLIN,
                revents: 0,
            };
            // SAFETY: one valid pollfd. The 10 s bound only turns a wedged
            // worker into a failure instead of a hung test run.
            let ready = unsafe { libc::poll(&mut pollfd, 1, 10_000) };
            assert!(ready > 0, "no seccomp notification or hang-up within 10 s");
            if pollfd.revents & libc::POLLIN == 0 {
                // POLLHUP: the filtered thread has exited.
                break;
            }
            // SAFETY: both structs are plain data; RECV requires a zeroed one.
            let mut notif: libc::seccomp_notif = unsafe { std::mem::zeroed() };
            if unsafe {
                libc::ioctl(
                    listener.as_raw_fd(),
                    libc::SECCOMP_IOCTL_NOTIF_RECV,
                    &mut notif,
                )
            } != 0
            {
                let error = std::io::Error::last_os_error();
                // ENOENT: the notifying call was interrupted before we read it.
                assert_eq!(
                    error.raw_os_error(),
                    Some(libc::ENOENT),
                    "SECCOMP_IOCTL_NOTIF_RECV: {error}"
                );
                continue;
            }
            let nr = libc::c_long::from(notif.data.nr);
            let slot = WATCHED
                .iter()
                .position(|watched| *watched == nr)
                .expect("the filter only reports watched syscalls");
            counts[slot] += 1;
            let mut resp: libc::seccomp_notif_resp = unsafe { std::mem::zeroed() };
            resp.id = notif.id;
            resp.flags = libc::SECCOMP_USER_NOTIF_FLAG_CONTINUE as u32;
            // SAFETY: `resp` is a valid response for the id just received.
            let sent = unsafe {
                libc::ioctl(
                    listener.as_raw_fd(),
                    libc::SECCOMP_IOCTL_NOTIF_SEND,
                    &mut resp,
                )
            };
            assert_eq!(
                sent,
                0,
                "SECCOMP_IOCTL_NOTIF_SEND: {}",
                std::io::Error::last_os_error()
            );
        }

        let sock = worker
            .join()
            .expect("the filtered thread must not panic")
            .expect("udp_connect must succeed toward a bound loopback backend");

        let expected = if cfg!(debug_assertions) {
            [1, 1, 1]
        } else {
            [0, 0, 0]
        };
        assert_eq!(
            counts,
            expected,
            "udp_connect (fcntl, getsockname, getpeername) calls, debug_assertions = {}",
            cfg!(debug_assertions)
        );

        let fd = sock.as_raw_fd();
        // SAFETY: fcntl queries on a descriptor we own.
        let (status, fd_flags) = unsafe {
            (
                libc::fcntl(fd, libc::F_GETFL),
                libc::fcntl(fd, libc::F_GETFD),
            )
        };
        assert!(status >= 0 && fd_flags >= 0, "fcntl on the returned socket");
        assert_ne!(
            status & libc::O_NONBLOCK,
            0,
            "the socket must be non-blocking"
        );
        assert_ne!(
            fd_flags & libc::FD_CLOEXEC,
            0,
            "the socket must be close-on-exec"
        );
        let local = sock
            .local_addr()
            .expect("the socket must report its local address");
        assert!(local.is_ipv4(), "the local family must match the backend's");
        assert_ne!(local.port(), 0, "connect must pick a concrete source port");
        assert_eq!(
            sock.peer_addr().expect("the socket must be connected"),
            backend,
            "connect must pin the peer to the backend"
        );
    }

    // ---- peer resets (sozu-proxy/sozu#434) ------------------------------

    /// The current value of one proxy-level counter on this test thread,
    /// absent counting as 0. `METRICS` is a `thread_local!` shared with every
    /// test libtest runs on the same thread, so callers assert a DELTA
    /// against a baseline, never an absolute value. Mirrors `proxy_metric`
    /// in `lib/src/protocol/mux/stream.rs`.
    pub(crate) fn proxy_count(name: &str) -> i64 {
        use sozu_command::proto::command::filtered_metrics::Inner;
        crate::metrics::METRICS.with(|metrics| {
            metrics
                .borrow_mut()
                .dump_local_proxy_metrics()
                .get(name)
                .and_then(|metric| match metric.inner {
                    Some(Inner::Count(value)) => Some(value),
                    _ => None,
                })
                .unwrap_or(0)
        })
    }

    /// A nonblocking loopback stream whose peer reset it: the accepted side
    /// sets `SO_LINGER` to zero and closes, which makes the kernel send a
    /// RST instead of a FIN.
    ///
    /// Returns once `poll(2)` reports `POLLERR` on the stream, so the RST has
    /// been processed and the next syscall is the one that answers
    /// `ECONNRESET`. `poll` does not consume the pending socket error, a
    /// blocking read would. Bounded: a RST that never arrives fails the test
    /// instead of hanging it.
    pub(crate) fn reset_stream() -> TcpStream {
        let listener = std::net::TcpListener::bind("127.0.0.1:0")
            .expect("test listener must bind to a loopback port");
        let stream = std::net::TcpStream::connect(
            listener
                .local_addr()
                .expect("test listener must report its local address"),
        )
        .expect("loopback connect must complete");
        let (peer, _) = listener.accept().expect("loopback accept must complete");
        socket2::SockRef::from(&peer)
            .set_linger(Some(std::time::Duration::ZERO))
            .expect("SO_LINGER must accept a zero timeout");
        drop(peer);

        let mut pollfd = libc::pollfd {
            fd: std::os::unix::io::AsRawFd::as_raw_fd(&stream),
            events: libc::POLLIN,
            revents: 0,
        };
        // SAFETY: one valid `pollfd` borrowed for the duration of the call.
        let ready = unsafe { libc::poll(&mut pollfd, 1, 5_000) };
        assert_eq!(ready, 1, "the peer's RST must arrive within 5 s");
        assert_ne!(
            pollfd.revents & libc::POLLERR,
            0,
            "premise: the peer must have reset the connection, not closed it"
        );
        stream
            .set_nonblocking(true)
            .expect("mio requires a nonblocking stream");
        TcpStream::from_std(stream)
    }

    /// A read that meets the peer's RST answers `Closed`, as it always did,
    /// and counts `tcp.read.reset` once: the kernel reports `ECONNRESET` to
    /// that read only, and the next one is an EOF.
    ///
    /// To SEE THIS RED: drop `incr!(names::tcp::READ_RESET)` from
    /// `plain_socket_read`.
    #[test]
    fn a_plain_read_counts_a_peer_reset_once() {
        let mut stream = reset_stream();
        let before = proxy_count(names::tcp::READ_RESET);
        let mut buf = [0u8; 64];

        let (size, result) = stream.socket_read(&mut buf);
        assert_eq!((size, result), (0, SocketResult::Closed));
        assert_eq!(proxy_count(names::tcp::READ_RESET) - before, 1);

        let (size, result) = stream.socket_read(&mut buf);
        assert_eq!((size, result), (0, SocketResult::Closed));
        assert_eq!(
            proxy_count(names::tcp::READ_RESET) - before,
            1,
            "the EOF after the reset is not another reset"
        );
    }

    /// A write that meets the peer's RST answers `Closed`, as it always did,
    /// still counts `tcp.write.error`, and counts `tcp.write.reset` once: the
    /// next write answers `EPIPE`, which is not a reset.
    ///
    /// To SEE THIS RED: drop `incr!(names::tcp::WRITE_RESET)` from
    /// `tcp_socket_write`.
    #[test]
    fn a_plain_write_counts_a_peer_reset_once() {
        let mut stream = reset_stream();
        let resets = proxy_count(names::tcp::WRITE_RESET);
        let errors = proxy_count(names::tcp::WRITE_ERROR);

        let (size, result) = stream.socket_write(b"after the reset");
        assert_eq!((size, result), (0, SocketResult::Closed));
        assert_eq!(proxy_count(names::tcp::WRITE_RESET) - resets, 1);
        assert_eq!(proxy_count(names::tcp::WRITE_ERROR) - errors, 1);

        let (size, result) = stream.socket_write(b"after the reset");
        assert_eq!((size, result), (0, SocketResult::Closed));
        assert_eq!(
            proxy_count(names::tcp::WRITE_RESET) - resets,
            1,
            "the EPIPE after the reset is not another reset"
        );
    }

    /// The vectored write path counts a reset exactly as the scalar one.
    ///
    /// To SEE THIS RED: drop `incr!(names::tcp::WRITE_RESET)` from
    /// `tcp_socket_write_vectored`.
    #[test]
    fn a_plain_vectored_write_counts_a_peer_reset() {
        let mut stream = reset_stream();
        let before = proxy_count(names::tcp::WRITE_RESET);

        let (size, result) =
            stream.socket_write_vectored(&[std::io::IoSlice::new(b"after the reset")]);
        assert_eq!((size, result), (0, SocketResult::Closed));
        assert_eq!(proxy_count(names::tcp::WRITE_RESET) - before, 1);
    }
}

/// [`rustls_socket_read`] and [`plain_socket_read`] against an in-memory
/// transport that counts every read a call issues: each one is a `recv(2)` on
/// the real socket.
#[cfg(test)]
pub(crate) mod rustls_read_tests {
    use std::{collections::VecDeque, io::Write, sync::Arc};

    use super::*;

    /// SNI the test certificate is registered under.
    pub(crate) const TEST_SNI: &str = "lolcatho.st";

    /// A frame header, then its payload: what the H2 read path asks for, one
    /// `socket_read` at a time (`lib/src/protocol/mux/h2.rs`, `readable_inner`).
    const FRAME_HEADER_LEN: usize = 9;

    /// The frames of a client's first flight after the preface — SETTINGS,
    /// WINDOW_UPDATE, HEADERS, PING — as `(header, payload)` byte blocks. Only
    /// their lengths matter to the transport layer; the contents are
    /// distinct so a misordered or duplicated read cannot compare equal.
    fn client_frames() -> Vec<Vec<u8>> {
        [6usize, 4, 37, 8]
            .iter()
            .enumerate()
            .flat_map(|(index, &payload_len)| {
                let mut header = vec![0u8; FRAME_HEADER_LEN];
                header[..3].copy_from_slice(&(payload_len as u32).to_be_bytes()[1..]);
                header[3] = index as u8;
                let payload = (0..payload_len)
                    .map(|byte| (index * 64 + byte) as u8)
                    .collect();
                [header, payload]
            })
            .collect()
    }

    /// A non-blocking transport: `wire` is what the kernel holds, served by at
    /// most one read, and `eof` is a received FIN once `wire` is drained.
    /// `eagains` counts the reads that found nothing, the ones a short read
    /// makes unnecessary. The first `interrupts` reads answer `EINTR`.
    #[derive(Default)]
    pub(crate) struct CountingTransport {
        pub(crate) wire: VecDeque<u8>,
        pub(crate) eof: bool,
        pub(crate) reset: bool,
        pub(crate) reads: usize,
        pub(crate) eagains: usize,
        pub(crate) interrupts: usize,
    }

    impl Read for CountingTransport {
        fn read(&mut self, buf: &mut [u8]) -> std::io::Result<usize> {
            self.reads += 1;
            if self.interrupts > 0 {
                self.interrupts -= 1;
                return Err(ErrorKind::Interrupted.into());
            }
            if self.reset {
                return Err(ErrorKind::ConnectionReset.into());
            }
            if self.wire.is_empty() {
                return if self.eof {
                    Ok(0)
                } else {
                    self.eagains += 1;
                    Err(ErrorKind::WouldBlock.into())
                };
            }
            let len = buf.len().min(self.wire.len());
            for (slot, byte) in buf.iter_mut().zip(self.wire.drain(..len)) {
                *slot = byte;
            }
            Ok(len)
        }
    }

    /// Accepts whatever certificate the server presents: the client is the
    /// encryption oracle for the bytes under test, not a party whose trust
    /// decision is being checked. `lib/assets/certificate.pem` carries no
    /// subjectAltName, so rustls's web-PKI verifier would refuse it.
    #[derive(Debug)]
    struct AcceptAnyServerCertificate;

    impl rustls::client::danger::ServerCertVerifier for AcceptAnyServerCertificate {
        fn verify_server_cert(
            &self,
            _end_entity: &rustls::pki_types::CertificateDer<'_>,
            _intermediates: &[rustls::pki_types::CertificateDer<'_>],
            _server_name: &rustls::pki_types::ServerName<'_>,
            _ocsp_response: &[u8],
            _now: rustls::pki_types::UnixTime,
        ) -> Result<rustls::client::danger::ServerCertVerified, rustls::Error> {
            Ok(rustls::client::danger::ServerCertVerified::assertion())
        }

        fn verify_tls12_signature(
            &self,
            _message: &[u8],
            _cert: &rustls::pki_types::CertificateDer<'_>,
            _dss: &rustls::DigitallySignedStruct,
        ) -> Result<rustls::client::danger::HandshakeSignatureValid, rustls::Error> {
            Ok(rustls::client::danger::HandshakeSignatureValid::assertion())
        }

        fn verify_tls13_signature(
            &self,
            _message: &[u8],
            _cert: &rustls::pki_types::CertificateDer<'_>,
            _dss: &rustls::DigitallySignedStruct,
        ) -> Result<rustls::client::danger::HandshakeSignatureValid, rustls::Error> {
            Ok(rustls::client::danger::HandshakeSignatureValid::assertion())
        }

        fn supported_verify_schemes(&self) -> Vec<rustls::SignatureScheme> {
            crate::crypto::default_provider()
                .signature_verification_algorithms
                .supported_schemes()
        }
    }

    /// Move every pending TLS byte from `from` to `to`, in memory.
    fn pump_tls<A, B>(
        from: &mut rustls::ConnectionCommon<A>,
        to: &mut rustls::ConnectionCommon<B>,
    ) {
        let mut wire = Vec::new();
        while from.wants_write() {
            from.write_tls(&mut wire)
                .expect("a TLS flight must serialize into memory");
        }
        let mut cursor = std::io::Cursor::new(wire.as_slice());
        while (cursor.position() as usize) < wire.len() {
            to.read_tls(&mut cursor)
                .expect("a TLS flight must be readable from memory");
            to.process_new_packets()
                .expect("a TLS flight must process cleanly");
        }
    }

    /// A TLS 1.3 server session serving the test certificate for
    /// [`TEST_SNI`], and a client that accepts it and offers `alpn`, neither
    /// started.
    pub(crate) fn fresh_pair(alpn: Vec<Vec<u8>>) -> (ServerConnection, rustls::ClientConnection) {
        let provider = Arc::new(crate::crypto::default_provider());
        let resolver = Arc::new(crate::tls::MutexCertificateResolver::default());
        resolver
            .0
            .lock()
            .expect("the test resolver lock must be available")
            .add_certificate(&sozu_command::proto::command::AddCertificate {
                address: sozu_command::proto::command::SocketAddress::new_v4(127, 0, 0, 1, 8443),
                certificate: sozu_command::proto::command::CertificateAndKey {
                    certificate: include_str!("../assets/certificate.pem").to_owned(),
                    key: include_str!("../assets/key.pem").to_owned(),
                    names: vec![TEST_SNI.to_owned()],
                    ..Default::default()
                },
                expired_at: None,
            })
            .expect("the test certificate must load into the resolver");
        let mut server_config = rustls::ServerConfig::builder_with_provider(provider.clone())
            .with_protocol_versions(&[&rustls::version::TLS13])
            .expect("the test provider must support TLS 1.3")
            .with_no_client_auth()
            .with_cert_resolver(resolver);
        server_config.send_tls13_tickets = 0;
        let client_config = rustls::ClientConfig::builder_with_provider(provider)
            .with_protocol_versions(&[&rustls::version::TLS13])
            .expect("the test provider must support TLS 1.3")
            .dangerous()
            .with_custom_certificate_verifier(Arc::new(AcceptAnyServerCertificate))
            .with_no_client_auth();
        let server = ServerConnection::new(Arc::new(server_config))
            .expect("the test server session must initialize");
        let client = rustls::ClientConnection::new_with_alpn(
            Arc::new(client_config),
            rustls::pki_types::ServerName::try_from(TEST_SNI)
                .expect("the test SNI must be a valid DNS name"),
            alpn,
        )
        .expect("the test client session must initialize");
        (server, client)
    }

    /// A settled TLS 1.3 server session and the client that feeds it.
    fn handshaken_pair() -> (ServerConnection, rustls::ClientConnection) {
        let (mut server, mut client) = fresh_pair(Vec::new());
        for _ in 0..16 {
            if !client.is_handshaking() && !server.is_handshaking() && !client.wants_write() {
                break;
            }
            pump_tls(&mut client, &mut server);
            pump_tls(&mut server, &mut client);
        }
        assert!(
            !server.is_handshaking() && !client.is_handshaking(),
            "premise: the in-memory handshake must complete"
        );
        (server, client)
    }

    /// Encrypt `plaintext` as ONE `write_all` + drain, so a flight smaller
    /// than a record's 16 KiB limit lands in a single TLS record.
    fn seal(client: &mut rustls::ClientConnection, plaintext: &[u8]) -> Vec<u8> {
        client
            .writer()
            .write_all(plaintext)
            .expect("the client must absorb the plaintext");
        let mut wire = Vec::new();
        while client.wants_write() {
            client
                .write_tls(&mut wire)
                .expect("the client record must serialize");
        }
        wire
    }

    /// The server side under test: a session plus the flags `FrontRustls`
    /// threads through [`rustls_socket_read`].
    struct Front {
        session: ServerConnection,
        peer_disconnected: bool,
        peer_reset: bool,
        tls_fatal: bool,
        memory: RecvMemory,
    }

    impl Front {
        fn read(
            &mut self,
            transport: &mut CountingTransport,
            len: usize,
        ) -> (Vec<u8>, SocketResult) {
            let mut buf = vec![0u8; len];
            let (size, result, fault) = rustls_socket_read(
                &mut self.session,
                transport,
                &mut buf,
                &mut self.peer_disconnected,
                &mut self.peer_reset,
                &mut self.tls_fatal,
                &mut self.memory,
            );
            assert!(fault.is_none(), "unexpected read fault: {fault:?}");
            buf.truncate(size);
            (buf, result)
        }

        /// Same as [`Self::read`], for a pass that is expected to fault.
        fn read_faulting(
            &mut self,
            transport: &mut CountingTransport,
            len: usize,
        ) -> (Vec<u8>, SocketResult) {
            let mut buf = vec![0u8; len];
            let (size, result, _fault) = rustls_socket_read(
                &mut self.session,
                transport,
                &mut buf,
                &mut self.peer_disconnected,
                &mut self.peer_reset,
                &mut self.tls_fatal,
                &mut self.memory,
            );
            buf.truncate(size);
            (buf, result)
        }

        /// What `FrontRustls::readiness_delivered` does when the event loop
        /// delivers `events` for the socket.
        fn deliver(&mut self, events: Ready) {
            self.memory.event_delivered(events);
        }
    }

    fn front_and_client() -> (Front, rustls::ClientConnection) {
        let (session, client) = handshaken_pair();
        (
            Front {
                session,
                peer_disconnected: false,
                peer_reset: false,
                tls_fatal: false,
                memory: RecvMemory::default(),
            },
            client,
        )
    }

    /// Several H2 frames in ONE TLS record, read the way the H2 path reads
    /// them: header, payload, header, payload. The record costs one `recv`;
    /// every later frame is already decrypted and must be served from
    /// rustls's plaintext buffer, and the call that finds that buffer empty
    /// answers `WouldBlock` without a `recv`: the first call's short `recv`
    /// proved the queue empty and no event was delivered since.
    ///
    /// Before #1606 every call started with `read_tls`, so each frame block
    /// after the first issued one `recv` that returned `EAGAIN`: 1 + 7 + 1.
    /// Before #1609 the memory did not outlive a call, and the last call still
    /// issued one: 1 + 1.
    #[test]
    fn frames_sharing_one_record_cost_one_recv() {
        let (mut front, mut client) = front_and_client();
        let frames = client_frames();
        let record = seal(&mut client, &frames.concat());
        let mut transport = CountingTransport {
            wire: record.into(),
            ..Default::default()
        };

        for (index, block) in frames.iter().enumerate() {
            let (read, result) = front.read(&mut transport, block.len());
            assert_eq!(
                &read, block,
                "frame block {index} must arrive intact and in order"
            );
            assert_eq!(result, SocketResult::Continue, "frame block {index}");
        }
        let (read, result) = front.read(&mut transport, FRAME_HEADER_LEN);
        assert!(read.is_empty(), "nothing is left after the last frame");
        assert_eq!(result, SocketResult::WouldBlock);

        assert_eq!(
            (transport.reads, transport.eagains),
            (1, 0),
            "one recv for the record and no EAGAIN once the plaintext is drained, \
             for {} frame blocks",
            frames.len()
        );
        assert!(!front.peer_disconnected && !front.peer_reset);
    }

    /// An event delivered after the proof clears it: the bytes that raised it
    /// are read by the next call, with no EAGAIN on either side.
    #[test]
    fn an_event_after_a_short_read_lets_the_next_call_read() {
        let (mut front, mut client) = front_and_client();
        let frames = client_frames();
        let mut transport = CountingTransport {
            wire: seal(&mut client, &frames.concat()).into(),
            ..Default::default()
        };
        for block in &frames {
            assert_eq!(&front.read(&mut transport, block.len()).0, block);
        }
        assert_eq!(
            front.read(&mut transport, FRAME_HEADER_LEN),
            (Vec::new(), SocketResult::WouldBlock),
            "no event since the short read: nothing is read"
        );

        transport.wire.extend(seal(&mut client, &frames[0]));
        front.deliver(Ready::READABLE);
        let (read, result) = front.read(&mut transport, frames[0].len());
        assert_eq!(read, frames[0], "the new arrival is read");
        assert_eq!(result, SocketResult::Continue);
        assert_eq!((transport.reads, transport.eagains), (2, 0));
    }

    /// A FIN whose edge is delivered after the proof, as HUP: the memory is
    /// off from then on, and the next call reads the EOF.
    #[test]
    fn a_hup_delivered_after_a_short_read_lets_the_next_call_read_the_eof() {
        let (mut front, mut client) = front_and_client();
        let frames = client_frames();
        let mut transport = CountingTransport {
            wire: seal(&mut client, &frames.concat()).into(),
            ..Default::default()
        };
        for block in &frames {
            assert_eq!(&front.read(&mut transport, block.len()).0, block);
        }

        transport.eof = true;
        front.deliver(Ready::HUP);
        assert_eq!(
            front.read(&mut transport, FRAME_HEADER_LEN),
            (Vec::new(), SocketResult::Closed)
        );
        assert!(front.peer_disconnected);
        assert_eq!((transport.reads, transport.eagains), (2, 0));
    }

    /// A caller buffer larger than everything received (the H1 path) takes the
    /// whole record in one short `recv` and stops there: the queue is empty,
    /// so a second `recv` could only answer EAGAIN.
    ///
    /// Before the stop on a short read the pass called `read_tls` again and
    /// counted 2 reads, the second one EAGAIN.
    #[test]
    fn a_large_buffer_drains_the_record_in_one_short_recv() {
        let (mut front, mut client) = front_and_client();
        let plaintext = client_frames().concat();
        let mut transport = CountingTransport {
            wire: seal(&mut client, &plaintext).into(),
            ..Default::default()
        };

        let (read, result) = front.read(&mut transport, 16 * 1024);
        assert_eq!(read, plaintext);
        assert_eq!(result, SocketResult::WouldBlock);
        assert_eq!(
            (transport.reads, transport.eagains),
            (1, 0),
            "one short recv for the record and no EAGAIN"
        );
    }

    /// `close_notify` in the same arrival as the data, read with a large
    /// buffer: the pass stops reading the socket after the short `recv`, but
    /// must still report the close rustls already processed, in the same call.
    /// A pass that skipped every `read_tls` after a short read would answer
    /// `WouldBlock` here and leave the close unreported until a later event.
    #[test]
    fn close_notify_in_the_short_read_is_reported_in_the_same_call() {
        let (mut front, mut client) = front_and_client();
        let plaintext = client_frames().concat();
        let mut wire = seal(&mut client, &plaintext);
        client.send_close_notify();
        while client.wants_write() {
            client
                .write_tls(&mut wire)
                .expect("the close_notify alert must serialize");
        }
        let mut transport = CountingTransport {
            wire: wire.into(),
            ..Default::default()
        };

        let (read, result) = front.read(&mut transport, 16 * 1024);
        assert_eq!(read, plaintext, "the data precedes the close");
        assert_eq!(result, SocketResult::Closed);
        assert!(front.peer_disconnected && !front.peer_reset);
        assert_eq!((transport.reads, transport.eagains), (1, 0));
    }

    /// A TCP FIN behind the data, read with a large buffer: the short `recv`
    /// delivers the data and stops, and the FIN is read by the next call. The
    /// mux makes that call because it saw HUP
    /// (`update_readiness_after_read`).
    #[test]
    fn a_fin_behind_a_short_read_is_reported_by_the_next_call() {
        let (mut front, mut client) = front_and_client();
        let plaintext = client_frames().concat();
        let mut transport = CountingTransport {
            wire: seal(&mut client, &plaintext).into(),
            eof: true,
            ..Default::default()
        };
        // The FIN came with the data: one edge, delivered as HUP, which turns
        // the memory off so the next call reads the EOF.
        front.deliver(Ready::READABLE | Ready::HUP);

        let (read, result) = front.read(&mut transport, 16 * 1024);
        assert_eq!(read, plaintext);
        assert_eq!(result, SocketResult::WouldBlock);
        assert!(!front.peer_disconnected, "the FIN has not been read yet");

        let (read, result) = front.read(&mut transport, 16 * 1024);
        assert!(read.is_empty());
        assert_eq!(result, SocketResult::Closed);
        assert!(front.peer_disconnected && !front.peer_reset);
        assert_eq!((transport.reads, transport.eagains), (2, 0));
    }

    /// A body spanning several records and many 4096-byte `read_tls` calls,
    /// read with the H1 buffer size: every byte arrives in order and the full
    /// reads keep the pass going.
    ///
    /// No read answers EAGAIN. The final short `recv` happens in a call whose
    /// buffer then fills from the plaintext it decrypted, so that call answers
    /// `Continue`; the next call drains the rest and answers `WouldBlock`
    /// without a `recv`, because no event was delivered since the short one
    /// ([`RecvMemory`]). Before #1609 that call issued one `recv` for an
    /// EAGAIN.
    #[test]
    fn a_large_body_over_tls_arrives_intact() {
        let (mut front, mut client) = front_and_client();
        let body: Vec<u8> = (0..(64 * 1024 + 123)).map(|i| (i % 251) as u8).collect();
        // One record per 16 KiB chunk: the client's outgoing buffer would
        // refuse the whole body in one `write_all`.
        let mut transport = CountingTransport {
            wire: body
                .chunks(16 * 1024)
                .flat_map(|chunk| seal(&mut client, chunk))
                .collect(),
            ..Default::default()
        };

        let mut received = Vec::new();
        loop {
            let (read, result) = front.read(&mut transport, 16 * 1024);
            received.extend_from_slice(&read);
            match result {
                SocketResult::Continue => assert_eq!(read.len(), 16 * 1024),
                SocketResult::WouldBlock => break,
                other => panic!("unexpected {other:?} after {} bytes", received.len()),
            }
        }
        assert_eq!(received, body);
        assert!(transport.wire.is_empty());
        assert_eq!(
            transport.eagains, 0,
            "the call after the one that filled from the last short read reads nothing"
        );
    }

    /// A record split across two arrivals: the first `read_tls` returns bytes
    /// but `process_new_packets` produces no plaintext. That `recv` was short,
    /// so the queue is empty and the pass reports `WouldBlock` with nothing
    /// delivered, without a second `recv`. The rest of the record then
    /// completes it.
    #[test]
    fn a_fragmented_record_is_read_again_until_it_completes() {
        let (mut front, mut client) = front_and_client();
        let frames = client_frames();
        let record = seal(&mut client, &frames.concat());
        let (head, tail) = record.split_at(record.len() / 2);
        let mut transport = CountingTransport {
            wire: head.iter().copied().collect(),
            ..Default::default()
        };

        let (read, result) = front.read(&mut transport, FRAME_HEADER_LEN);
        assert!(read.is_empty(), "half a record decrypts to nothing");
        assert_eq!(result, SocketResult::WouldBlock);
        assert_eq!(
            (transport.reads, transport.eagains),
            (1, 0),
            "the partial record, in one short recv"
        );

        transport.wire.extend(tail);
        // The tail's arrival raises the edge the event loop delivers.
        front.deliver(Ready::READABLE);
        for (index, block) in frames.iter().enumerate() {
            let (read, result) = front.read(&mut transport, block.len());
            assert_eq!(&read, block, "frame block {index}");
            assert_eq!(result, SocketResult::Continue, "frame block {index}");
        }
        assert_eq!(transport.reads, 2, "the rest of the record costs one recv");
    }

    /// `close_notify` behind the data in the same flight: every frame is
    /// still delivered, then the pass reports `Closed` — from rustls's own
    /// `read_tls` early return, without another `recv`.
    #[test]
    fn close_notify_after_the_frames_reports_closed_once_they_are_read() {
        let (mut front, mut client) = front_and_client();
        let frames = client_frames();
        let mut wire = seal(&mut client, &frames.concat());
        client.send_close_notify();
        while client.wants_write() {
            client
                .write_tls(&mut wire)
                .expect("the close_notify alert must serialize");
        }
        let mut transport = CountingTransport {
            wire: wire.into(),
            ..Default::default()
        };

        for (index, block) in frames.iter().enumerate() {
            let (read, result) = front.read(&mut transport, block.len());
            assert_eq!(&read, block, "frame block {index}");
            assert_eq!(result, SocketResult::Continue, "frame block {index}");
        }
        let (read, result) = front.read(&mut transport, FRAME_HEADER_LEN);
        assert!(read.is_empty());
        assert_eq!(
            result,
            SocketResult::Closed,
            "close_notify must close the read side"
        );
        assert!(
            front.peer_disconnected,
            "a clean close is a read-side disconnect"
        );
        assert!(!front.peer_reset, "a clean close is not a reset");
        assert_eq!(
            transport.reads, 1,
            "close_notify needs no recv beyond the flight"
        );

        let (read, result) = front.read(&mut transport, FRAME_HEADER_LEN);
        assert!(read.is_empty());
        assert_eq!(
            result,
            SocketResult::Closed,
            "a closed session stays closed"
        );
    }

    /// A TCP FIN without `close_notify`: buffered frames are delivered first,
    /// then `Closed` (never `Error`), and a later call is `Closed` again.
    #[test]
    fn tcp_eof_without_close_notify_reports_closed_after_the_buffered_frames() {
        let (mut front, mut client) = front_and_client();
        let frames = client_frames();
        let mut transport = CountingTransport {
            wire: seal(&mut client, &frames.concat()).into(),
            eof: true,
            ..Default::default()
        };
        // The FIN came with the data: one edge, delivered as HUP.
        front.deliver(Ready::READABLE | Ready::HUP);

        for (index, block) in frames.iter().enumerate() {
            let (read, result) = front.read(&mut transport, block.len());
            assert_eq!(&read, block, "frame block {index}");
            assert_eq!(result, SocketResult::Continue, "frame block {index}");
        }
        let (read, result) = front.read(&mut transport, FRAME_HEADER_LEN);
        assert!(read.is_empty());
        assert_eq!(
            result,
            SocketResult::Closed,
            "a FIN must close the read side"
        );
        assert!(front.peer_disconnected && !front.peer_reset);

        let (read, result) = front.read(&mut transport, FRAME_HEADER_LEN);
        assert!(read.is_empty());
        assert_eq!(result, SocketResult::Closed, "EOF stays EOF, not an error");
    }

    /// A valid record of four frames followed, in the same arrival, by a
    /// corrupted one. `process_new_packets` appends the first record's
    /// plaintext and then fails on the second: the session is dead. Every
    /// later call must answer `Error` without serving that plaintext, or the
    /// mux (which only drops READABLE on `Error`) would parse frames from a
    /// TLS session that has already failed as soon as the peer sends again.
    #[test]
    fn a_fatal_tls_error_stays_an_error_and_serves_no_buffered_plaintext() {
        let (mut front, mut client) = front_and_client();
        let frames = client_frames();
        let mut wire = seal(&mut client, &frames.concat());
        let mut corrupted = seal(&mut client, b"after the valid record");
        let last = corrupted.len() - 1;
        corrupted[last] ^= 0xff;
        wire.extend(corrupted);
        let mut transport = CountingTransport {
            wire: wire.into(),
            ..Default::default()
        };

        let (read, result) = front.read_faulting(&mut transport, FRAME_HEADER_LEN);
        assert!(read.is_empty(), "the failing pass delivers nothing");
        assert_eq!(result, SocketResult::Error);

        for call in 0..2 * frames.len() {
            transport.wire.extend([0u8; 16]);
            let (read, result) = front.read_faulting(&mut transport, FRAME_HEADER_LEN);
            assert_eq!(
                (read.len(), result),
                (0, SocketResult::Error),
                "call {call} after a fatal TLS error must stay (0, Error)"
            );
        }
    }

    /// The pass that hits the fatal error keeps what it had already copied:
    /// plaintext served from the buffer BEFORE `read_tls` met the corrupt
    /// record is returned with `Error` (`(n, Error)`), and only later passes
    /// are `(0, Error)`.
    #[test]
    fn plaintext_served_before_the_fatal_error_is_returned_with_it() {
        let (mut front, mut client) = front_and_client();
        let frames = client_frames();
        let plaintext = frames.concat();
        let mut transport = CountingTransport {
            wire: seal(&mut client, &plaintext).into(),
            ..Default::default()
        };
        let (read, result) = front.read(&mut transport, FRAME_HEADER_LEN);
        assert_eq!(read, plaintext[..FRAME_HEADER_LEN]);
        assert_eq!(result, SocketResult::Continue);

        let mut corrupted = seal(&mut client, b"after the valid record");
        let last = corrupted.len() - 1;
        corrupted[last] ^= 0xff;
        transport.wire.extend(corrupted);
        front.deliver(Ready::READABLE);

        let (read, result) = front.read_faulting(&mut transport, 16 * 1024);
        assert_eq!(
            read,
            plaintext[FRAME_HEADER_LEN..],
            "the rest of the valid record"
        );
        assert_eq!(result, SocketResult::Error);
        let (read, result) = front.read_faulting(&mut transport, 16 * 1024);
        assert_eq!((read.len(), result), (0, SocketResult::Error));
    }

    /// A reset is `Closed` with `peer_reset` set, so writes short-circuit.
    #[test]
    fn a_reset_reports_closed_and_marks_the_peer_reset() {
        let (mut front, _client) = front_and_client();
        let mut transport = CountingTransport {
            reset: true,
            ..Default::default()
        };

        let (read, result) = front.read(&mut transport, FRAME_HEADER_LEN);
        assert!(read.is_empty());
        assert_eq!(result, SocketResult::Closed);
        assert!(front.peer_disconnected && front.peer_reset);
        assert_eq!(transport.reads, 1);
    }

    /// A settled TLS frontend over a real socket its peer reset
    /// (`SO_LINGER` zero, then close).
    fn reset_front() -> FrontRustls {
        let (session, _client) = handshaken_pair();
        FrontRustls {
            stream: super::tests::reset_stream(),
            session,
            peer_disconnected: false,
            peer_reset: false,
            tls_fatal: false,
            session_ulid: Ulid::generate(),
            configured_peer: None,
            recv_memory: RecvMemory::default(),
        }
    }

    /// A TLS read that meets the peer's RST answers `Closed` and counts
    /// `rustls.read.reset` once. The write that follows short-circuits on
    /// `peer_reset` without reaching the socket, so it counts nothing, and
    /// the reset does not leak into the plain-TCP counter either.
    ///
    /// To SEE THIS RED: drop `incr!(names::rustls::READ_RESET)` from
    /// `rustls_socket_read`.
    #[test]
    fn a_tls_read_counts_a_peer_reset_once() {
        let mut front = reset_front();
        let reads = super::tests::proxy_count(names::rustls::READ_RESET);
        let writes = super::tests::proxy_count(names::rustls::WRITE_RESET);
        let plain = super::tests::proxy_count(names::tcp::READ_RESET);

        let mut buf = [0u8; 64];
        let (size, result) = front.socket_read(&mut buf);
        assert_eq!((size, result), (0, SocketResult::Closed));
        assert!(front.peer_reset);
        assert_eq!(
            super::tests::proxy_count(names::rustls::READ_RESET) - reads,
            1
        );

        let (_, result) = front.socket_write(b"after the reset");
        assert_eq!(result, SocketResult::Closed);
        assert_eq!(
            super::tests::proxy_count(names::rustls::WRITE_RESET) - writes,
            0,
            "a write after a reset read never reaches the socket"
        );
        assert_eq!(super::tests::proxy_count(names::tcp::READ_RESET) - plain, 0);
    }

    /// A TLS write that meets the peer's RST answers `Closed`, still counts
    /// `rustls.write.error`, and counts `rustls.write.reset` once: the next
    /// write short-circuits on `peer_reset`.
    ///
    /// To SEE THIS RED: drop `incr!(names::rustls::WRITE_RESET)` from
    /// `flush_tls`.
    #[test]
    fn a_tls_write_counts_a_peer_reset_once() {
        let mut front = reset_front();
        let resets = super::tests::proxy_count(names::rustls::WRITE_RESET);
        let errors = super::tests::proxy_count(names::rustls::WRITE_ERROR);

        let (_, result) = front.socket_write(b"after the reset");
        assert_eq!(result, SocketResult::Closed);
        assert!(front.peer_reset);
        assert_eq!(
            super::tests::proxy_count(names::rustls::WRITE_RESET) - resets,
            1
        );
        assert_eq!(
            super::tests::proxy_count(names::rustls::WRITE_ERROR) - errors,
            1
        );

        let (_, result) = front.socket_write_vectored(&[std::io::IoSlice::new(b"after the reset")]);
        assert_eq!(result, SocketResult::Closed);
        assert_eq!(
            super::tests::proxy_count(names::rustls::WRITE_RESET) - resets,
            1,
            "the reset is counted once"
        );
    }

    /// A settled TLS frontend over a socket whose connection the kernel
    /// refused: its first write answers `ECONNREFUSED`, which no arm but the
    /// generic one names.
    fn refused_front() -> FrontRustls {
        let address = std::net::TcpListener::bind("127.0.0.1:0")
            .expect("test listener must bind to a loopback port")
            .local_addr()
            .expect("test listener must report its local address");
        // The listener is gone: the connect below meets a RST.
        let socket =
            Socket::new(Domain::IPV4, Type::STREAM, None).expect("a TCP socket must be created");
        socket
            .set_nonblocking(true)
            .expect("mio requires a nonblocking stream");
        if let Err(e) = socket.connect(&address.into()) {
            assert_eq!(
                e.raw_os_error(),
                Some(libc::EINPROGRESS),
                "a nonblocking connect must be in progress, got {e}"
            );
        }
        let mut pollfd = libc::pollfd {
            fd: std::os::unix::io::AsRawFd::as_raw_fd(&socket),
            events: libc::POLLOUT,
            revents: 0,
        };
        // SAFETY: one valid `pollfd` borrowed for the duration of the call.
        let ready = unsafe { libc::poll(&mut pollfd, 1, 5_000) };
        assert_eq!(ready, 1, "the refusal must arrive within 5 s");
        assert_ne!(
            pollfd.revents & libc::POLLERR,
            0,
            "premise: the kernel must have refused the connection"
        );
        let (session, _client) = handshaken_pair();
        FrontRustls {
            stream: TcpStream::from_std(std::net::TcpStream::from(socket)),
            session,
            peer_disconnected: false,
            peer_reset: false,
            tls_fatal: false,
            session_ulid: Ulid::generate(),
            configured_peer: None,
            recv_memory: RecvMemory::default(),
        }
    }

    /// A TLS write that meets a socket error no other arm names answers
    /// `Error` and marks the channel dead: the record rustls still holds can
    /// never be delivered, so `socket_wants_write` stops reporting it, and the
    /// next write short-circuits without reaching the socket. Otherwise the
    /// record kept the H1 session's flush pending, which delays its close on
    /// the hang-up, and a mux pass that re-raised WRITABLE for it retried the
    /// failing write (sozu-proxy/sozu#1780).
    ///
    /// To SEE THIS RED: drop `self.peer_reset = true;` from the generic arm
    /// of the `flush_tls` loop in `FrontRustls::socket_write`.
    #[test]
    fn a_tls_write_that_meets_a_socket_error_stops_wanting_to_write() {
        let mut front = refused_front();
        let errors = super::tests::proxy_count(names::rustls::WRITE_ERROR);

        let (_, result) = front.socket_write(b"refused by the kernel");
        assert_eq!(result, SocketResult::Error);
        assert_eq!(
            super::tests::proxy_count(names::rustls::WRITE_ERROR) - errors,
            1
        );
        assert!(
            front.session.wants_write(),
            "premise: rustls must still hold the record the kernel refused"
        );
        assert!(
            !front.socket_wants_write(),
            "a record no write can deliver must not read as a pending write"
        );

        let writes = tls_writes();
        let (_, result) = front.socket_write_vectored(&[std::io::IoSlice::new(b"after the error")]);
        assert_eq!(result, SocketResult::Closed);
        assert_eq!(
            tls_writes(),
            writes,
            "a write after the error must not reach the socket"
        );
    }

    /// `refused_front` with records already queued in rustls and nothing
    /// written yet, for the empty-buffer flushes that only drain them.
    fn refused_front_holding_records() -> FrontRustls {
        let mut front = refused_front();
        front
            .session
            .writer()
            .write_all(b"queued before the flush")
            .expect("rustls must absorb the plaintext");
        assert!(
            front.session.wants_write(),
            "premise: rustls must hold the record the flush will try to write"
        );
        front
    }

    /// The write that met the socket error answered `Error`, and the record
    /// rustls still holds no longer reads as a pending write.
    fn assert_error_marks_the_channel_dead(front: &FrontRustls, result: SocketResult) {
        assert_eq!(result, SocketResult::Error);
        assert!(
            front.session.wants_write(),
            "premise: rustls must still hold the record the kernel refused"
        );
        assert!(
            front.peer_reset && !front.socket_wants_write(),
            "an Error write must mark the channel dead, or the record keeps the \
             session waiting for a WRITABLE edge that never comes"
        );
    }

    /// The empty-buffer flush of `socket_write` — the one `H2Shell` issues to
    /// push queued records — marks the channel dead on a socket error.
    ///
    /// TO SEE THIS RED: drop the `self.peer_reset = true;` of the `is_error`
    /// return in `FrontRustls::socket_write`.
    #[test]
    fn an_empty_tls_flush_that_meets_a_socket_error_marks_the_channel_dead() {
        let mut front = refused_front_holding_records();
        let (_, result) = front.socket_write(&[]);
        assert_error_marks_the_channel_dead(&front, result);
    }

    /// A vectored write rustls absorbs whole, then flushes in the main loop.
    ///
    /// TO SEE THIS RED: drop the `self.peer_reset = true;` of the `is_error`
    /// return in `FrontRustls::socket_write_vectored`.
    #[test]
    fn a_vectored_tls_write_that_meets_a_socket_error_marks_the_channel_dead() {
        let mut front = refused_front();
        let (size, result) = front.socket_write_vectored(&[
            std::io::IoSlice::new(b"refused "),
            std::io::IoSlice::new(b"by the kernel"),
        ]);
        assert_eq!(size, 21, "premise: rustls must absorb the whole write");
        assert_error_marks_the_channel_dead(&front, result);
    }

    /// A vectored write rustls absorbs only in part (its send buffer is
    /// capped), which takes the partial-write flush before returning.
    ///
    /// TO SEE THIS RED: the same edit as the whole-write test above.
    #[test]
    fn a_partial_vectored_tls_write_that_meets_a_socket_error_marks_the_channel_dead() {
        let mut front = refused_front();
        front.session.set_buffer_limit(Some(64));
        let payload = vec![b'x'; 4096];
        let (size, result) = front.socket_write_vectored(&[std::io::IoSlice::new(&payload)]);
        assert!(
            size > 0 && size < payload.len(),
            "premise: rustls must absorb part of the write, got {size}"
        );
        assert_error_marks_the_channel_dead(&front, result);
    }

    /// The empty-buffer flush of `socket_write_vectored`.
    ///
    /// TO SEE THIS RED: the same edit as the whole-write test above.
    #[test]
    fn an_empty_vectored_tls_flush_that_meets_a_socket_error_marks_the_channel_dead() {
        let mut front = refused_front_holding_records();
        let (_, result) = front.socket_write_vectored(&[]);
        assert_error_marks_the_channel_dead(&front, result);
    }

    /// A transport whose first write is interrupted (`EINTR`) and whose next
    /// one takes everything.
    #[derive(Default)]
    struct InterruptedOnce {
        calls: usize,
        written: Vec<u8>,
    }

    impl std::io::Write for InterruptedOnce {
        fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
            self.calls += 1;
            if self.calls == 1 {
                return Err(std::io::Error::from(ErrorKind::Interrupted));
            }
            self.written.extend_from_slice(buf);
            Ok(buf.len())
        }

        fn flush(&mut self) -> std::io::Result<()> {
            Ok(())
        }
    }

    /// An interrupted TLS write is retried, not reported: as `WouldBlock` no
    /// edge would resume it, and as an error it would mark the channel dead.
    ///
    /// TO SEE THIS RED: remove the `Interrupted => continue` arm of the retry
    /// loop in `flush_tls`.
    #[test]
    fn an_interrupted_tls_write_is_retried() {
        let (mut session, _client) = handshaken_pair();
        session
            .writer()
            .write_all(b"interrupted once")
            .expect("rustls must absorb the plaintext");
        let mut transport = InterruptedOnce::default();

        let written = flush_tls(&mut session, &mut transport)
            .expect("an interrupted write must be retried, not reported");

        assert!(written > 0 && transport.written.len() == written);
        assert_eq!(transport.calls, 2, "one interrupted write, one retry");
        assert!(!session.wants_write(), "the retry must deliver the record");
    }

    /// An interrupted plain TCP write is retried, not reported: as an error it
    /// would close the session (the pipe path closes on a front write error).
    ///
    /// TO SEE THIS RED: remove the `Interrupted` arm of the write loop in
    /// `plain_socket_write`.
    #[test]
    fn an_interrupted_plain_write_is_retried() {
        let mut transport = InterruptedOnce::default();

        let (size, result, fault) = plain_socket_write(&mut transport, b"interrupted once");

        assert!(fault.is_none(), "unexpected write fault: {fault:?}");
        assert_eq!((size, result), (16, SocketResult::Continue));
        assert_eq!(transport.calls, 2, "one interrupted write, one retry");
        assert_eq!(transport.written, b"interrupted once");
    }

    /// The vectored plain TCP write retries an interruption the same way.
    ///
    /// TO SEE THIS RED: remove the `Interrupted` arm of the retry loop in
    /// `plain_socket_write_vectored`.
    #[test]
    fn an_interrupted_plain_vectored_write_is_retried() {
        let mut transport = InterruptedOnce::default();
        let bufs = [std::io::IoSlice::new(b"interrupted once")];

        let (size, result, fault) = plain_socket_write_vectored(&mut transport, &bufs);

        assert!(fault.is_none(), "unexpected write fault: {fault:?}");
        assert_eq!((size, result), (16, SocketResult::Continue));
        assert_eq!(transport.calls, 2, "one interrupted write, one retry");
        assert_eq!(transport.written, b"interrupted once");
    }

    /// An interrupted plain TCP read is retried, not reported as an error.
    ///
    /// TO SEE THIS RED: remove the `Interrupted` arm of the retry loop in
    /// `plain_socket_read`.
    #[test]
    fn an_interrupted_plain_read_is_retried() {
        let mut transport = CountingTransport {
            wire: b"interrupted once".iter().copied().collect(),
            interrupts: 1,
            ..Default::default()
        };
        let mut buf = [0u8; 32];

        let (size, result, fault) = plain_socket_read(&mut transport, &mut buf);

        assert!(fault.is_none(), "unexpected read fault: {fault:?}");
        assert_eq!((size, result), (16, SocketResult::WouldBlock));
        assert_eq!(&buf[..size], b"interrupted once");
        assert_eq!(transport.reads, 2, "one interrupted read, one retry");
    }

    /// An interrupted `recv` under `read_tls` is retried, not reported as a
    /// `ReadTls` fault.
    ///
    /// TO SEE THIS RED: remove the `Interrupted` arm of the `read_tls` retry
    /// loop in `rustls_socket_read`.
    #[test]
    fn an_interrupted_tls_read_is_retried() {
        let (mut front, mut client) = front_and_client();
        let mut transport = CountingTransport {
            wire: seal(&mut client, b"interrupted once").into(),
            interrupts: 1,
            ..Default::default()
        };

        let (read, result) = front.read(&mut transport, 32);

        assert_eq!(read, b"interrupted once");
        assert_eq!(result, SocketResult::WouldBlock);
        assert_eq!(transport.reads, 2, "one interrupted read, one retry");
    }

    /// Read everything `transport` holds through [`plain_socket_read`] with a
    /// `len`-byte buffer, one call at a time, until a call answers something
    /// other than `Continue`.
    fn plain_read_all(transport: &mut CountingTransport, len: usize) -> (Vec<u8>, SocketResult) {
        let mut received = Vec::new();
        loop {
            let mut buf = vec![0u8; len];
            let (size, result, fault) = plain_socket_read(transport, &mut buf);
            assert!(fault.is_none(), "unexpected read fault: {fault:?}");
            received.extend_from_slice(&buf[..size]);
            if result != SocketResult::Continue {
                return (received, result);
            }
            assert_eq!(size, len, "Continue means the buffer filled");
        }
    }

    /// A request smaller than the buffer (the H1 path, one request per
    /// connection) costs one `recv`: it answers fewer bytes than offered, the
    /// queue is empty, and the call reports `WouldBlock` with the bytes.
    ///
    /// Before the stop on a short read `tcp_socket_read` looped and counted 2
    /// reads, the second one EAGAIN.
    #[test]
    fn a_short_plain_read_costs_one_recv_and_no_eagain() {
        let request = b"GET /index.html HTTP/1.1\r\nHost: lolcatho.st\r\n\r\n".to_vec();
        let mut transport = CountingTransport {
            wire: request.iter().copied().collect(),
            ..Default::default()
        };

        let mut buf = vec![0u8; 16 * 1024];
        let (size, result, fault) = plain_socket_read(&mut transport, &mut buf);
        assert!(fault.is_none());
        assert_eq!(&buf[..size], &request[..]);
        assert_eq!(result, SocketResult::WouldBlock);
        assert_eq!(
            (transport.reads, transport.eagains),
            (1, 0),
            "one short recv and no EAGAIN"
        );
    }

    /// A read that fills the buffer is not a short read: it answers
    /// `Continue`, READABLE stays, and the next call reads on.
    #[test]
    fn a_full_plain_buffer_is_not_a_short_read() {
        let mut transport = CountingTransport {
            wire: (0..250u8).collect(),
            ..Default::default()
        };

        let mut buf = vec![0u8; 100];
        let (size, result, _) = plain_socket_read(&mut transport, &mut buf);
        assert_eq!((size, result), (100, SocketResult::Continue));
        let (size, result, _) = plain_socket_read(&mut transport, &mut buf);
        assert_eq!((size, result), (100, SocketResult::Continue));
        let (size, result, _) = plain_socket_read(&mut transport, &mut buf);
        assert_eq!((size, result), (50, SocketResult::WouldBlock));
        assert_eq!((transport.reads, transport.eagains), (3, 0));
    }

    /// Data followed by a FIN: the short read delivers the data, and the next
    /// call reads the EOF as `Closed`. An empty queue with a FIN is `Closed`
    /// at once.
    #[test]
    fn a_fin_behind_a_short_plain_read_is_closed_on_the_next_call() {
        let mut transport = CountingTransport {
            wire: (0..50u8).collect(),
            eof: true,
            ..Default::default()
        };

        let mut buf = vec![0u8; 16 * 1024];
        let (size, result, _) = plain_socket_read(&mut transport, &mut buf);
        assert_eq!((size, result), (50, SocketResult::WouldBlock));
        assert_eq!(buf[..size], (0..50u8).collect::<Vec<_>>()[..]);
        let (size, result, _) = plain_socket_read(&mut transport, &mut buf);
        assert_eq!((size, result), (0, SocketResult::Closed));
        let (size, result, _) = plain_socket_read(&mut transport, &mut buf);
        assert_eq!((size, result), (0, SocketResult::Closed), "EOF stays EOF");
        assert_eq!(transport.eagains, 0);
    }

    /// A message arriving in fragments: each arrival costs one short `recv`
    /// and ends its call, and the bytes concatenate in order.
    #[test]
    fn a_fragmented_plain_message_costs_one_recv_per_arrival() {
        let message: Vec<u8> = (0..70u8).collect();
        let mut transport = CountingTransport::default();
        let mut received = Vec::new();

        for chunk in message.chunks(30) {
            transport.wire.extend(chunk);
            let mut buf = vec![0u8; 16 * 1024];
            let (size, result, _) = plain_socket_read(&mut transport, &mut buf);
            assert_eq!((size, result), (chunk.len(), SocketResult::WouldBlock));
            received.extend_from_slice(&buf[..size]);
        }
        assert_eq!(received, message);
        assert_eq!((transport.reads, transport.eagains), (3, 0));
    }

    /// A body much larger than the buffer: every full read answers `Continue`,
    /// the final short read answers `WouldBlock`, every byte arrives in order,
    /// and no read answers EAGAIN.
    #[test]
    fn a_large_plain_body_arrives_intact_without_eagain() {
        let body: Vec<u8> = (0..(1024 * 1024 + 123)).map(|i| (i % 251) as u8).collect();
        let mut transport = CountingTransport {
            wire: body.iter().copied().collect(),
            ..Default::default()
        };

        let (received, result) = plain_read_all(&mut transport, 16 * 1024);
        assert_eq!(result, SocketResult::WouldBlock);
        assert_eq!(received, body);
        assert_eq!(
            (transport.reads, transport.eagains),
            (65, 0),
            "64 full reads and the short tail"
        );
    }

    /// A body that is an exact multiple of the buffer ends on a full read, so
    /// the only way to learn the queue is empty is the EAGAIN that follows: a
    /// full buffer is never taken for a short read.
    #[test]
    fn a_plain_body_ending_on_a_full_buffer_still_needs_its_eagain() {
        let body: Vec<u8> = (0..(4 * 16 * 1024)).map(|i| (i % 251) as u8).collect();
        let mut transport = CountingTransport {
            wire: body.iter().copied().collect(),
            ..Default::default()
        };

        let (received, result) = plain_read_all(&mut transport, 16 * 1024);
        assert_eq!(result, SocketResult::WouldBlock);
        assert_eq!(received, body);
        assert_eq!((transport.reads, transport.eagains), (5, 1));
    }

    /// The kernel property the stop relies on, on a real socket registered
    /// edge-triggered with mio: after a short read, bytes that arrive later
    /// raise a new READABLE event, and a FIN raises one too. No event is
    /// pending in between, so nothing else would have woken the reader.
    #[test]
    fn after_a_short_read_new_bytes_raise_a_new_edge() {
        use std::io::Write as _;

        let token = mio::Token(7);
        let mut poll = mio::Poll::new().expect("a poll instance must open");
        let mut events = mio::Events::with_capacity(8);
        let listener = std::net::TcpListener::bind("127.0.0.1:0")
            .expect("test listener must bind to a loopback port");
        let mut client = std::net::TcpStream::connect(
            listener
                .local_addr()
                .expect("test listener must report its local address"),
        )
        .expect("loopback connect must complete");
        let (accepted, _) = listener.accept().expect("the connection must be accepted");
        accepted
            .set_nonblocking(true)
            .expect("mio requires a nonblocking stream");
        let mut server = TcpStream::from_std(accepted);
        poll.registry()
            .register(&mut server, token, mio::Interest::READABLE)
            .expect("the stream must register");
        let wait_for_edge = |poll: &mut mio::Poll, events: &mut mio::Events| {
            poll.poll(events, Some(std::time::Duration::from_secs(5)))
                .expect("poll must succeed");
            events
                .iter()
                .any(|event| event.token() == token && event.is_readable())
        };
        let mut buf = vec![0u8; 16 * 1024];

        client.write_all(b"first").expect("the client must write");
        assert!(wait_for_edge(&mut poll, &mut events), "the first arrival");
        assert_eq!(
            tcp_socket_read(&mut server, &mut buf, None, None),
            (5, SocketResult::WouldBlock)
        );

        poll.poll(&mut events, Some(std::time::Duration::from_millis(50)))
            .expect("poll must succeed");
        assert!(events.is_empty(), "edge-triggered: nothing new, no event");

        client.write_all(b"second").expect("the client must write");
        assert!(
            wait_for_edge(&mut poll, &mut events),
            "bytes arriving after a short read must raise a new edge"
        );
        assert_eq!(
            tcp_socket_read(&mut server, &mut buf, None, None),
            (6, SocketResult::WouldBlock)
        );

        client
            .shutdown(std::net::Shutdown::Write)
            .expect("the client must half-close");
        assert!(wait_for_edge(&mut poll, &mut events), "the FIN");
        assert_eq!(
            tcp_socket_read(&mut server, &mut buf, None, None),
            (0, SocketResult::Closed)
        );
    }
}
