use std::{
    io::{ErrorKind, Read, Write},
    net::SocketAddr,
};

use mio::net::{TcpListener, TcpStream};
use rustls::{ProtocolVersion, ServerConnection};
use rusty_ulid::Ulid;
use socket2::{Domain, Protocol, Socket, Type};
use sozu_command::{config::MAX_LOOP_ITERATIONS, logging::ansi_palette};

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
    fn protocol(&self) -> TransportProtocol;
    fn read_error(&self);
    fn write_error(&self);
    /// Returns the owning connection's session ULID when known. Used by
    /// [`log_socket_context!`] to render the `[<session_ulid> - - -]` segment
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
/// matches the rest of the mux log-context macros: `SOCKET` bold bright-white,
/// `Session` light grey, keys gray, values bright white.
///
/// `local` is a live `getsockname(2)` lookup and is always populated for a
/// bound socket. `rtt` + `state` come from a single `getsockopt(TCP_INFO)`
/// call via [`socket_snapshot`]; both render as `None` on an FSM state where
/// the kernel rejects the call (e.g. just-reset sockets).
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
            peer = $self.socket_ref().peer_addr().ok(),
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
/// [`TcpStream`] through as parameters. Renders the same `[<ulid> - - -]\t
/// SOCKET\tSession(peer=..., local=..., rtt=..., state=..., protocol=Tcp)
/// \t >>>` prefix as [`log_socket_context!`] so free-function error logs
/// align column-by-column with handler-backed ones.
///
/// When the ULID is `None` the slot is rendered as `-`. `peer` prefers the
/// caller-supplied `configured_peer` (stored at [`SessionTcpStream`]
/// construction, immune to ENOTCONN on a socket that failed an asynchronous
/// `connect()`) and falls back to a live `getpeername(2)` lookup when no
/// cache is available. `local` is a `getsockname(2)` lookup which stays
/// valid across failed connects (the bind is local). `rtt` and `state`
/// come from a single `getsockopt(TCP_INFO)` call — `state="SYN_SENT"` is
/// the clearest signal for a failed outbound `connect()`. Protocol is
/// hardcoded to `Tcp` because the helper is only called from the raw-TCP
/// `tcp_socket_*` free functions.
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
            incr!("socket.read.infinite_loop.error");
            return (size, SocketResult::Error);
        }
        if size == buf.len() {
            return (size, SocketResult::Continue);
        }
        match stream.read(&mut buf[size..]) {
            Ok(0) => return (size, SocketResult::Closed),
            Ok(sz) => size += sz,
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
            incr!("socket.write.infinite_loop.error");
            return (size, SocketResult::Error);
        }
        if size == buf.len() {
            return (size, SocketResult::Continue);
        }
        match stream.write(&buf[size..]) {
            Ok(0) => return (size, SocketResult::Continue),
            Ok(sz) => size += sz,
            Err(e) => match e.kind() {
                ErrorKind::WouldBlock => return (size, SocketResult::WouldBlock),
                ErrorKind::ConnectionReset
                | ErrorKind::ConnectionAborted
                | ErrorKind::BrokenPipe
                | ErrorKind::ConnectionRefused => {
                    incr!("tcp.write.error");
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
                    incr!("tcp.write.error");
                    return (size, SocketResult::Error);
                }
                _ => {
                    //FIXME: timeout and other common errors should be sent up
                    error!(
                        "{} socket_write error={:?}",
                        log_socket_module_prefix(stream, session_ulid, configured_peer),
                        e
                    );
                    incr!("tcp.write.error");
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
        Ok(sz) => (sz, SocketResult::Continue),
        Err(e) => match e.kind() {
            ErrorKind::WouldBlock => (0, SocketResult::WouldBlock),
            ErrorKind::ConnectionReset
            | ErrorKind::ConnectionAborted
            | ErrorKind::BrokenPipe
            | ErrorKind::ConnectionRefused => {
                incr!("tcp.write.error");
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
                incr!("tcp.write.error");
                (0, SocketResult::Error)
            }
            _ => {
                //FIXME: timeout and other common errors should be sent up
                error!(
                    "{} socket_write error={:?}",
                    log_socket_module_prefix(stream, session_ulid, configured_peer),
                    e
                );
                incr!("tcp.write.error");
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

    fn protocol(&self) -> TransportProtocol {
        TransportProtocol::Tcp
    }

    fn read_error(&self) {
        incr!("tcp.read.error");
    }

    fn write_error(&self) {
        incr!("tcp.write.error");
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
    /// Peer address cached at construction. Unlike a live `getpeername(2)`
    /// lookup, this survives a socket that failed an asynchronous `connect()`
    /// (Linux surfaces ENOTCONN via `getpeername` in that state) and is
    /// therefore the reliable source of truth for the `peer=` slot in
    /// [`log_socket_module_prefix`] when ECONNREFUSED fires on the first
    /// read/write — the case that motivated this cache.
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

    fn protocol(&self) -> TransportProtocol {
        TransportProtocol::Tcp
    }

    fn read_error(&self) {
        incr!("tcp.read.error");
    }

    fn write_error(&self) {
        incr!("tcp.write.error");
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
                incr!("rustls.read.infinite_loop.error");
                is_error = true;
                break;
            }

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
                incr!("rustls.write.infinite_loop.error");
                is_error = true;
                break;
            }
            if buffered_size == buf.len() {
                break;
            }

            if !can_write | is_error | is_closed {
                break;
            }

            match self.session.writer().write(&buf[buffered_size..]) {
                Ok(0) => {} // zero byte written means that the Rustls buffers are full, we will try to write on the socket and try again
                Ok(sz) => {
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
                        incr!("rustls.write.error");
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
                        incr!("rustls.write.error");
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
                            incr!("rustls.write.error");
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
                            incr!("rustls.write.error");
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
                            incr!("rustls.write.error");
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
                            incr!("rustls.write.error");
                            is_error = true;
                            break;
                        }
                    },
                }
            }
        }

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
                incr!("rustls.write.infinite_loop.error");
                is_error = true;
                break;
            }
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
                        buffered_size += sz;
                    }
                    Err(e) => match e.kind() {
                        ErrorKind::WouldBlock => {}
                        ErrorKind::ConnectionReset
                        | ErrorKind::ConnectionAborted
                        | ErrorKind::BrokenPipe => {
                            incr!("rustls.write.error");
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
                            incr!("rustls.write.error");
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
                                incr!("rustls.write.error");
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
                                incr!("rustls.write.error");
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
                            incr!("rustls.write.error");
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
                            incr!("rustls.write.error");
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
                            incr!("rustls.write.error");
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
                            incr!("rustls.write.error");
                            is_error = true;
                            break;
                        }
                    },
                }
            }
        }

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
        incr!("rustls.read.error");
    }

    fn write_error(&self) {
        incr!("rustls.write.error");
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

    Ok(TcpListener::from_std(sock.into()))
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
        let mut tcp_info: TcpInfo = unsafe { std::mem::zeroed() };
        let mut len = std::mem::size_of::<TcpInfo>() as libc::socklen_t;
        let status = unsafe {
            libc::getsockopt(
                fd,
                OPT_LEVEL,
                OPT_NAME,
                &mut tcp_info as *mut _ as *mut _,
                &mut len,
            )
        };
        if status != 0 { None } else { Some(tcp_info) }
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
