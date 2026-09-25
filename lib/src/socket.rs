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
use sozu_command::{config::MAX_LOOP_ITERATIONS, logging::ansi_palette};

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

/// Shared read/write/vectored-write logic used by both
/// [`impl SocketHandler for TcpStream`] and
/// [`impl SocketHandler for SessionTcpStream`]. Free-function entry point:
/// `self` is out of scope here, so error logs use [`log_socket_module_prefix`]
/// which renders the same `Session(peer, rtt, protocol)` context as
/// [`log_socket_context!`] by reading from the `stream` + `session_ulid`
/// parameters threaded through each helper.
fn tcp_socket_read(
    stream: &mut TcpStream,
    buf: &mut [u8],
    session_ulid: Option<Ulid>,
    configured_peer: Option<SocketAddr>,
) -> (usize, SocketResult) {
    let mut size = 0usize;
    let mut counter = 0;
    loop {
        counter += 1;
        if counter > MAX_LOOP_ITERATIONS {
            error!(
                "{} MAX_LOOP_ITERATION reached in TcpStream::socket_read",
                log_socket_module_prefix(stream, session_ulid, configured_peer)
            );
            incr!(names::socket::READ_INFINITE_LOOP_ERROR);
            return (size, SocketResult::Error);
        }
        // Loop invariant: the running cursor never overshoots the buffer, so the
        // `&mut buf[size..]` slice below can never panic on a bad offset.
        debug_assert!(
            size <= buf.len(),
            "read cursor {size} overran buffer len {} (would slice out of bounds)",
            buf.len()
        );
        if size == buf.len() {
            return (size, SocketResult::Continue);
        }
        match stream.read(&mut buf[size..]) {
            Ok(0) => return (size, SocketResult::Closed),
            Ok(sz) => {
                // `read` cannot report more bytes than the slice it was given.
                debug_assert!(
                    sz <= buf.len() - size,
                    "read reported {sz} bytes into a {}-byte remaining slice",
                    buf.len() - size
                );
                size += sz;
            }
            Err(e) => match e.kind() {
                ErrorKind::WouldBlock => return (size, SocketResult::WouldBlock),
                // Treat `ConnectionRefused` as a closed socket, mirroring the
                // write path. On Linux a failed asynchronous `connect()`
                // surfaces as `ECONNREFUSED` on the first read; it is
                // operationally identical to any other benign peer-initiated
                // close and does not warrant a log line on every backend
                // that happens to be down.
                ErrorKind::ConnectionReset
                | ErrorKind::ConnectionAborted
                | ErrorKind::BrokenPipe
                | ErrorKind::ConnectionRefused => return (size, SocketResult::Closed),
                // Noisy-expected transport failures: backend unreachable,
                // TCP_USER_TIMEOUT expiry, post-close reads. Keep a log line
                // so operators can still trend the rate, but `warn!` — this
                // is reality-at-scale, not a sozu invariant break.
                ErrorKind::HostUnreachable
                | ErrorKind::NetworkUnreachable
                | ErrorKind::TimedOut
                | ErrorKind::NotConnected => {
                    warn!(
                        "{} socket_read error={:?}",
                        log_socket_module_prefix(stream, session_ulid, configured_peer),
                        e
                    );
                    return (size, SocketResult::Error);
                }
                // Genuinely loud variants (`PermissionDenied`, `AddrNotAvailable`,
                // `InvalidInput`/`Data`, …) and the unknown catch-all stay at
                // `error!` so operators keep paging on real misconfig.
                _ => {
                    error!(
                        "{} socket_read error={:?}",
                        log_socket_module_prefix(stream, session_ulid, configured_peer),
                        e
                    );
                    return (size, SocketResult::Error);
                }
            },
        }
    }
}

fn tcp_socket_write(
    stream: &mut TcpStream,
    buf: &[u8],
    session_ulid: Option<Ulid>,
    configured_peer: Option<SocketAddr>,
) -> (usize, SocketResult) {
    let mut size = 0usize;
    let mut counter = 0;
    loop {
        counter += 1;
        if counter > MAX_LOOP_ITERATIONS {
            error!(
                "{} MAX_LOOP_ITERATION reached in TcpStream::socket_write",
                log_socket_module_prefix(stream, session_ulid, configured_peer)
            );
            incr!(names::socket::WRITE_INFINITE_LOOP_ERROR);
            return (size, SocketResult::Error);
        }
        // Loop invariant: the cursor never overshoots the buffer, so the
        // `&buf[size..]` slice below can never panic on a bad offset.
        debug_assert!(
            size <= buf.len(),
            "write cursor {size} overran buffer len {} (would slice out of bounds)",
            buf.len()
        );
        if size == buf.len() {
            return (size, SocketResult::Continue);
        }
        match stream.write(&buf[size..]) {
            Ok(0) => return (size, SocketResult::Continue),
            Ok(sz) => {
                // `write` cannot report more bytes than the slice it was given.
                debug_assert!(
                    sz <= buf.len() - size,
                    "write reported {sz} bytes from a {}-byte remaining slice",
                    buf.len() - size
                );
                size += sz;
            }
            Err(e) => match e.kind() {
                ErrorKind::WouldBlock => return (size, SocketResult::WouldBlock),
                ErrorKind::ConnectionReset
                | ErrorKind::ConnectionAborted
                | ErrorKind::BrokenPipe
                | ErrorKind::ConnectionRefused => {
                    incr!(names::tcp::WRITE_ERROR);
                    return (size, SocketResult::Closed);
                }
                // Noisy-expected transport failures (see `tcp_socket_read`
                // for rationale). Log at `warn!` and still bump the
                // `tcp.write.error` counter so rate-based dashboards stay
                // accurate.
                ErrorKind::HostUnreachable
                | ErrorKind::NetworkUnreachable
                | ErrorKind::TimedOut
                | ErrorKind::NotConnected => {
                    warn!(
                        "{} socket_write error={:?}",
                        log_socket_module_prefix(stream, session_ulid, configured_peer),
                        e
                    );
                    incr!(names::tcp::WRITE_ERROR);
                    return (size, SocketResult::Error);
                }
                _ => {
                    //FIXME: timeout and other common errors should be sent up
                    error!(
                        "{} socket_write error={:?}",
                        log_socket_module_prefix(stream, session_ulid, configured_peer),
                        e
                    );
                    incr!(names::tcp::WRITE_ERROR);
                    return (size, SocketResult::Error);
                }
            },
        }
    }
}

fn tcp_socket_write_vectored(
    stream: &mut TcpStream,
    bufs: &[std::io::IoSlice],
    session_ulid: Option<Ulid>,
    configured_peer: Option<SocketAddr>,
) -> (usize, SocketResult) {
    match stream.write_vectored(bufs) {
        Ok(sz) => {
            // `write_vectored` cannot report more bytes than the slices held.
            debug_assert!(
                sz <= bufs.iter().map(|b| b.len()).sum::<usize>(),
                "write_vectored reported {sz} bytes from {}-byte slices",
                bufs.iter().map(|b| b.len()).sum::<usize>()
            );
            (sz, SocketResult::Continue)
        }
        Err(e) => match e.kind() {
            ErrorKind::WouldBlock => (0, SocketResult::WouldBlock),
            ErrorKind::ConnectionReset
            | ErrorKind::ConnectionAborted
            | ErrorKind::BrokenPipe
            | ErrorKind::ConnectionRefused => {
                incr!(names::tcp::WRITE_ERROR);
                (0, SocketResult::Closed)
            }
            // Noisy-expected transport failures (see `tcp_socket_read` for
            // rationale). Same tiering as the scalar write path.
            ErrorKind::HostUnreachable
            | ErrorKind::NetworkUnreachable
            | ErrorKind::TimedOut
            | ErrorKind::NotConnected => {
                warn!(
                    "{} socket_write error={:?}",
                    log_socket_module_prefix(stream, session_ulid, configured_peer),
                    e
                );
                incr!(names::tcp::WRITE_ERROR);
                (0, SocketResult::Error)
            }
            _ => {
                //FIXME: timeout and other common errors should be sent up
                error!(
                    "{} socket_write error={:?}",
                    log_socket_module_prefix(stream, session_ulid, configured_peer),
                    e
                );
                incr!(names::tcp::WRITE_ERROR);
                (0, SocketResult::Error)
            }
        },
    }
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
    /// Peer reset the connection (RST/ConnectionAborted/BrokenPipe). The TCP
    /// channel is dead; further writes are pointless and should short-circuit.
    pub peer_reset: bool,
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
}

impl std::fmt::Debug for FrontRustls {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("FrontRustls")
            .field("stream", &self.stream)
            .finish_non_exhaustive()
    }
}

impl SocketHandler for FrontRustls {
    fn socket_read(&mut self, buf: &mut [u8]) -> (usize, SocketResult) {
        let mut size = 0usize;
        let mut can_read = true;
        let mut is_error = false;
        let mut is_closed = false;

        let mut counter = 0;
        loop {
            counter += 1;
            if counter > MAX_LOOP_ITERATIONS {
                error!(
                    "{} MAX_LOOP_ITERATION reached in FrontRustls::socket_read",
                    log_socket_context!(self)
                );
                incr!(names::rustls::READ_INFINITE_LOOP_ERROR);
                is_error = true;
                break;
            }

            // Loop invariant: the plaintext cursor never overshoots the caller's
            // buffer, so every `&mut buf[size..]` below is a valid slice.
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

            match self.session.read_tls(&mut self.stream) {
                Ok(0) => {
                    // Graceful FIN on the read side: peer closed its write
                    // half. Keep `peer_reset` unset so outbound writes can
                    // still flush rustls's buffered records (half-close).
                    can_read = false;
                    is_closed = true;
                    self.peer_disconnected = true;
                }
                Ok(_sz) => {}
                Err(e) => match e.kind() {
                    ErrorKind::WouldBlock => {
                        can_read = false;
                    }
                    ErrorKind::ConnectionReset
                    | ErrorKind::ConnectionAborted
                    | ErrorKind::BrokenPipe => {
                        // Full RST/abort: the TCP channel is dead. Mark
                        // `peer_reset` so writes short-circuit (nothing can
                        // reach the peer anymore) but still set
                        // `peer_disconnected` for back-compatible read-side
                        // logic.
                        is_closed = true;
                        self.peer_disconnected = true;
                        self.peer_reset = true;
                    }
                    // https://github.com/rustls/rustls/blob/main/rustls/src/conn.rs#L482-L500
                    // rustls's 16 KB received_plaintext buffer is full — expected
                    // under H2 where frame-at-a-time reads drain less than a full
                    // TLS record. The outer loop will drain plaintext next iteration.
                    ErrorKind::Other => {}
                    _ => {
                        error!(
                            "{} could not read TLS stream from socket: {:?}",
                            log_socket_context!(self),
                            e
                        );
                        is_error = true;
                        break;
                    }
                },
            }

            if let Err(e) = self.session.process_new_packets() {
                error!(
                    "{} could not process read TLS packets: {:?}",
                    log_socket_context!(self),
                    e
                );
                is_error = true;
                break;
            }

            while !self.session.wants_read() {
                match self.session.reader().read(&mut buf[size..]) {
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
                            error!(
                                "{} could not read data from TLS stream: {:?}",
                                log_socket_context!(self),
                                e
                            );
                            is_error = true;
                            break;
                        }
                    },
                }
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
        if is_error {
            (size, SocketResult::Error)
        } else if is_closed {
            (size, SocketResult::Closed)
        } else if size == buf.len() {
            // The full requested amount was read (possibly from the rustls
            // plaintext buffer). Report Continue so the caller keeps
            // READABLE in the readiness set — there may be more decrypted
            // data available without a new mio event.
            (size, SocketResult::Continue)
        } else if !can_read {
            (size, SocketResult::WouldBlock)
        } else {
            (size, SocketResult::Continue)
        }
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
                match self.session.write_tls(&mut self.stream) {
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
                match self.session.write_tls(&mut self.stream) {
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
                    match self.session.write_tls(&mut self.stream) {
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
                match self.session.write_tls(&mut self.stream) {
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
                match self.session.write_tls(&mut self.stream) {
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

    fn socket_wants_write(&self) -> bool {
        // Only a true RST stops us wanting to write — a peer FIN still
        // allows flushing TLS plaintext buffered in rustls (half-close).
        !self.peer_reset && self.session.wants_write()
    }

    fn socket_ref(&self) -> &TcpStream {
        &self.stream
    }

    fn socket_mut(&mut self) -> &mut TcpStream {
        &mut self.stream
    }

    /// The fallback is reachable, and only the expect-proxy route makes it
    /// look otherwise. There, `upgrade_expect` returns `None` unless both
    /// PROXY addresses parse, so `peer_address` really is `Some` by the time
    /// a handshake exists. The direct route has no such guarantee:
    /// `HttpsSession::new` seeds `peer_address` with a best-effort
    /// `sock.peer_addr().ok()` at accept (`https.rs`), an `Option` precisely
    /// because that `getpeername(2)` can fail. Do not delete this arm —
    /// `https::tests::front_rustls_peer_addr_falls_back_to_the_live_lookup`
    /// fails without it.
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

pub fn server_bind(addr: SocketAddr) -> Result<TcpListener, ServerBindError> {
    let sock = Socket::new(Domain::for_address(addr), Type::STREAM, Some(Protocol::TCP))
        .map_err(ServerBindError::SocketCreationError)?;

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
pub fn udp_bind(addr: SocketAddr) -> Result<UdpSocket, ServerBindError> {
    let sock = Socket::new(Domain::for_address(addr), Type::DGRAM, Some(Protocol::UDP))
        .map_err(ServerBindError::SocketCreationError)?;

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
    let sock = Socket::new(
        Domain::for_address(backend),
        Type::DGRAM,
        Some(Protocol::UDP),
    )
    .map_err(ServerBindError::SocketCreationError)?;

    sock.bind(&unspecified.into())
        .map_err(ServerBindError::BindError)?;
    sock.set_nonblocking(true)
        .map_err(ServerBindError::SetNonBlocking)?;
    // `connect` on a DGRAM socket pins the return 4-tuple; a non-blocking
    // connect on UDP completes immediately (no handshake).
    sock.connect(&backend.into())
        .map_err(ServerBindError::BindError)?;

    // Post-conditions — assert the flag/connect state stuck (logic bug if not),
    // degrading to a no-op when the kernel refuses the query so a dying fd never
    // panics on this network-facing path.
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
mod tests {
    use super::*;

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
}
