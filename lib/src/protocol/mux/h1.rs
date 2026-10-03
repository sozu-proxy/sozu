//! H1 mux connection wrapper.
//!
//! Hosts the single active H1 stream (`stream: GlobalStreamId`) and wires
//! Kawa-owned H1 parsing + serialization into the shared mux `Context` so the
//! same routing / shutdown / readiness machinery applies across H1 and H2
//! connections. Long-form lifecycle: `lib/src/protocol/mux/LIFECYCLE.md`.

use std::{
    cell::Cell,
    io::IoSlice,
    time::{Duration, Instant},
};

use rusty_ulid::Ulid;
use sozu_command::{logging::ansi_palette, ready::Ready};

pub(super) use super::shared::{LINGER_MAX_BYTES, Linger};
use crate::metrics::names;
use crate::{
    L7ListenerHandler, ListenerHandler, Readiness,
    protocol::mux::{
        BackendStatus, Context, DebugEvent, Endpoint, GlobalStreamId, MuxResult, Position,
        StreamState, forcefully_terminate_answer, memoized_rtt,
        parser::H2Error,
        remove_backend_stream, set_default_answer,
        shared::{
            EndStreamAction, LingerRead, drain_discard, drain_tls_close_notify, end_stream_decision,
        },
        update_readiness_after_read, update_readiness_after_write,
    },
    socket::{SocketHandler, SocketResult},
};

/// Prefix applied to every [`ConnectionH1`] log line. Matches the RUSTLS
/// log-context convention (`MUX-H1\tSession(...)\t >>>`). When the logger is
/// in colored mode the label is bold bright-white (uniform across every
/// protocol) and the session detail is rendered in light grey.
///
/// Fields included in the session block (chosen to surface the most common
/// H1 troubleshooting axes — keep-alive churn, stream pinning, buffer-pressure
/// stall and graceful TLS shutdown):
/// - `peer` — peer address via [`SocketHandler::peer_addr`](crate::socket::SocketHandler::peer_addr),
///   snapshotted once into `ConnectionH1::peer_address` at construction rather
///   than looked up live on each expansion. It therefore survives the peer's
///   RST (which is when these lines are read); on a PROXY-protocol frontend it
///   names the advertised client rather than the load balancer, matching the
///   `HTTPS`/`HTTP` line for the same request id; and on a backend connection
///   it names the cluster-configured address even while the async `connect()`
///   is still in flight and `getpeername(2)` would refuse
/// - `position` — `Server` / `Client(...)` orientation
/// - `stream` — currently active [`GlobalStreamId`] (or `none`)
/// - `requests` — request count served on this connection (keep-alive)
/// - `parked` — set when the kawa buffer is full and `READABLE` is suspended
/// - `close_notify` — TLS `close_notify` send state
/// - `readiness` — connection-level mio readiness snapshot
macro_rules! log_context {
    ($self:expr) => {{
        let (open, reset, grey, gray, white) = ansi_palette();
        format!(
            "[{ulid} - - -]\t{open}MUX-H1{reset}\t{grey}Session{reset}({gray}peer{reset}={white}{peer:?}{reset}, {gray}position{reset}={white}{position:?}{reset}, {gray}stream{reset}={white}{stream:?}{reset}, {gray}requests{reset}={white}{requests}{reset}, {gray}parked{reset}={white}{parked}{reset}, {gray}close_notify{reset}={white}{close_notify}{reset}, {gray}readiness{reset}={white}{readiness}{reset})\t >>>",
            open = open,
            reset = reset,
            grey = grey,
            gray = gray,
            white = white,
            ulid = $self.session_ulid,
            peer = $self.peer_address,
            position = $self.position,
            stream = $self.stream,
            requests = $self.requests,
            parked = $self.parked_on_buffer_pressure,
            close_notify = $self.close_notify_sent,
            readiness = $self.readiness,
        )
    }};
}

/// Per-stream variant of [`log_context!`] used when a `HttpContext` is in
/// scope. Fills the `request_id` slot of the bracket so the log line can be
/// grepped by the specific request that triggered it.
#[allow(unused_macros)]
macro_rules! log_context_stream {
    ($self:expr, $http_context:expr) => {{
        let (open, reset, grey, gray, white) = ansi_palette();
        format!(
            "[{ulid} {req} {cluster} {backend}]\t{open}MUX-H1{reset}\t{grey}Session{reset}({gray}peer{reset}={white}{peer:?}{reset}, {gray}position{reset}={white}{position:?}{reset}, {gray}stream{reset}={white}{stream:?}{reset}, {gray}requests{reset}={white}{requests}{reset}, {gray}parked{reset}={white}{parked}{reset}, {gray}close_notify{reset}={white}{close_notify}{reset}, {gray}readiness{reset}={white}{readiness}{reset})\t >>>",
            open = open,
            reset = reset,
            grey = grey,
            gray = gray,
            white = white,
            ulid = $self.session_ulid,
            req = $http_context.id,
            cluster = $http_context.cluster_id.as_deref().unwrap_or("-"),
            backend = $http_context.backend_id.as_deref().unwrap_or("-"),
            peer = $self.peer_address,
            position = $self.position,
            stream = $self.stream,
            requests = $self.requests,
            parked = $self.parked_on_buffer_pressure,
            close_notify = $self.close_notify_sent,
            readiness = $self.readiness,
        )
    }};
}

/// Module-level prefix for logs without a [`ConnectionH1`] in scope. Honours
/// the colored flag.
macro_rules! log_module_context {
    () => {{
        let (open, reset, _, _, _) = ansi_palette();
        format!("{open}MUX-H1{reset}\t >>>", open = open, reset = reset)
    }};
}

/// HTTP/1.1 connection handler within the mux layer.
///
/// Manages a single HTTP/1.1 connection (either frontend or backend),
/// handling request/response forwarding through kawa buffers. Supports
/// keep-alive, chunked transfer encoding, close-delimited responses,
/// and upgrade (e.g., WebSocket).
pub struct ConnectionH1<Front: SocketHandler> {
    pub position: Position,
    pub readiness: Readiness,
    pub requests: usize,
    pub socket: Front,
    /// Peer address of this connection, captured once at construction from
    /// [`SocketHandler::peer_addr`](crate::socket::SocketHandler::peer_addr)
    /// and never re-read.
    ///
    /// This is the `peer=` slot every `log_context!` line carries, and the one
    /// its per-stream twin `log_context_stream!` fills. Both used to build it
    /// from `socket.socket_ref().peer_addr().ok()` — a live `getpeername(2)`
    /// that reached past the trait method added to stop exactly that.
    ///
    /// Both production handlers *prefer* a cached address and fall back to a
    /// live lookup when they hold none: `SessionTcpStream` and `FrontRustls`
    /// each answer `self.configured_peer.or_else(|| self.stream.peer_addr().ok())`
    /// (`socket.rs`). **That fallback arm is reachable** — the direct routes
    /// seed `configured_peer` from a best-effort `peer_addr().ok()` at accept,
    /// an `Option` precisely because that lookup can fail — it is pinned by its
    /// own tests, and nothing here licenses deleting it.
    ///
    /// The snapshot is at least as good on every path, which is the actual
    /// argument. It is taken while the connection is established, so wherever
    /// the fallback would have answered it answers here too; it then survives
    /// the peer's reset, where a later live lookup returns `ENOTCONN` and
    /// renders `peer=None` on exactly the error lines an operator reads during
    /// an incident. The divergence is one-directional — `Some` where `None`
    /// used to appear, never a different address.
    ///
    /// This is `ConnectionH2::peer_address`' shape, so an operator grepping a
    /// single session ULID reads the same `peer=` on the `MUX-H1` and `MUX-H2`
    /// lines of one connection.
    pub(super) peer_address: Option<std::net::SocketAddr>,
    /// Active stream index, or `None` when the connection has no assigned stream
    /// (initial client state before `start_stream`, or after `end_stream` detaches).
    pub stream: Option<GlobalStreamId>,
    /// Configured idle timeout for this connection. The core never arms a
    /// wheel entry itself: it publishes the next instant it wants to be called
    /// back at through `ConnectionH1::poll_timeout`, and the embedder — the
    /// `Mux` adapter — owns the `TimeoutContainer` that reflects it onto the
    /// real timer. See `LIFECYCLE.md` §7.7.
    pub timeout_duration: Duration,
    /// Next instant this connection wants `timeout()` called at, or `None`
    /// when it wants no timer at all.
    pub(super) timeout_deadline: Option<Instant>,
    /// Set when `readable` exits early because the kawa buffer was full.
    /// Edge-triggered epoll will not re-fire READABLE for data already in the
    /// kernel socket buffer, so the cross-readiness mechanism must re-arm it
    /// via `try_resume_reading` once the peer drains the buffer.
    pub parked_on_buffer_pressure: bool,
    /// A write to the client failed (reset, broken pipe or another socket
    /// error): nothing more can be flushed to it, so the connection reports
    /// no pending write and the session closes instead of waiting to flush.
    pub write_failed: bool,
    /// True once we've asked rustls to emit TLS close_notify for this frontend.
    pub close_notify_sent: bool,
    /// Connection/session ULID propagated from the parent [`super::Mux`]. Used to
    /// stamp the session slot of the `[session req cluster backend]` log
    /// prefix emitted by the local `log_context!` macro.
    pub session_ulid: Ulid,
    /// True once this backend connection has served a stream that it picked
    /// up out of the H1 keep-alive pool, rather than on the fresh dial that
    /// created it.
    ///
    /// This is pingora's `client_reused` bit — the provenance the protocol
    /// layer cannot see but the retry decision needs.
    ///
    /// It does NOT prove what the peer did. An EOF with no response is
    /// indistinguishable at this layer from a stale pool socket, from an
    /// origin that half-closed after processing the request but before
    /// answering, and from one that crashed mid-request: `readable` treats
    /// every `size == 0` alike, and an empty back buffer proves only that no
    /// response byte arrived. What this bit does is bound the window in
    /// which sozu is willing to re-issue: a connection that came out of the
    /// pool has been idle long enough for its peer to have closed it
    /// unobserved, a freshly dialled one has not. What makes re-issuing
    /// permissible at all is the idempotence gate (RFC 9110 §9.2.2) in
    /// `Stream::can_replay_on_fresh_upstream`; this bit only narrows when
    /// that permission is exercised, so only a reused connection arms
    /// request capture (sozu-proxy/sozu#1442). Always false on a
    /// `Position::Server` connection.
    pub reused_from_pool: bool,
    /// `writable`'s vectored-write scratch, kept for the connection's
    /// lifetime so a write pass reuses its capacity instead of allocating
    /// and growing a fresh vector (sozu-proxy/sozu#1580).
    ///
    /// Empty outside one `h2_transmit::gather` / `h2_transmit::confirm`
    /// bracket inside `writable`: only the capacity survives a pass, never a
    /// descriptor. `pub(super)` rather than private (as in `H2Shell`) only
    /// because `ConnectionH1` has no constructor in this module: both are
    /// struct literals in `connection.rs`. Nothing else in `mux` touches it.
    pub(super) io_slices: Vec<IoSlice<'static>>,
    /// This connection's one `getsockopt(TCP_INFO)` sample, `None` until its
    /// first access log asks for it. Read through `Connection::rtt` and
    /// [`Self::rtt`], both of which go through
    /// [`memoized_rtt`](super::memoized_rtt); never reset, so a keep-alive
    /// connection pays one read, not one per request. `pub(super)` for the
    /// same reason as `io_slices`: the struct literals live in
    /// `connection.rs`.
    pub(super) rtt: Cell<Option<Option<Duration>>>,
    /// How long a lingering close may drain the rest of a request: the
    /// listener's `request_timeout`, which a frontend connection is built
    /// with. Unused on a backend connection.
    pub(super) linger_timeout: Duration,
    /// Lingering-close state of a frontend connection. See [`Linger`].
    pub(super) linger: Linger,
}

impl<Front: SocketHandler> std::fmt::Debug for ConnectionH1<Front> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ConnectionH1")
            .field("position", &self.position)
            .field("readiness", &self.readiness)
            .field("peer_address", &self.peer_address)
            .field("stream", &self.stream)
            .field("reused_from_pool", &self.reused_from_pool)
            .finish()
    }
}

impl<Front: SocketHandler> ConnectionH1<Front> {
    /// Smoothed RTT of this connection's socket, sampled at the first access
    /// log that asks for it and reused for the connection's whole life.
    fn rtt(&self) -> Option<Duration> {
        memoized_rtt(&self.rtt, self.socket.socket_ref())
    }

    /// The next instant this connection wants its embedder to call `timeout()`
    /// at, or `None` for "no timer". The adapter reflects this onto the real
    /// wheel; nothing here touches `crate::timer`.
    pub(super) fn poll_timeout(&self) -> Option<Instant> {
        self.timeout_deadline
    }

    /// Push the deadline one full [`Self::timeout_duration`] out from `now`.
    /// Replaces the old `TimeoutContainer::reset()` at the same call sites.
    ///
    /// A no-op while lingering: the linger deadline bounds the whole drain,
    /// and every caller — `readable`, `writable`, `Mux::timeout_inner`'s
    /// re-arm after a write pass that may itself have started the linger —
    /// must leave it in place.
    pub(super) fn arm_timeout(&mut self, now: Instant) {
        if self.is_lingering() {
            return;
        }
        self.timeout_deadline = now.checked_add(self.timeout_duration);
    }

    /// Ask for no timer at all. Replaces `TimeoutContainer::cancel()`.
    pub(super) fn clear_timeout(&mut self) {
        self.timeout_deadline = None;
    }

    /// Adopt a new configured duration and re-arm from `now`.
    ///
    /// Mirrors `TimeoutContainer::set_duration`, which likewise cancelled and
    /// re-armed rather than keeping the old deadline. It re-arms
    /// unconditionally: the old method re-armed whenever the container had ever
    /// held a token, which every live connection has.
    pub(super) fn set_timeout_duration(&mut self, duration: Duration, now: Instant) {
        self.timeout_duration = duration;
        self.arm_timeout(now);
    }

    fn defer_close_for_tls_flush(&mut self, reason: &'static str) -> MuxResult {
        if self.initiate_close_notify() {
            trace!(
                "{} H1 writable delaying close after {}: stream={:?}, close_notify_sent={}, wants_write={}, readiness={:?}",
                log_context!(self),
                reason,
                self.stream,
                self.close_notify_sent,
                self.socket.socket_wants_write(),
                self.readiness
            );
            MuxResult::Continue
        } else if let Linger::Pending { deadline } = self.linger {
            self.start_linger(deadline, reason)
        } else {
            MuxResult::CloseSession
        }
    }

    /// Whether the connection is draining the rest of a request after its
    /// response (see [`Linger`]).
    pub(super) fn is_lingering(&self) -> bool {
        matches!(self.linger, Linger::Draining { .. })
    }

    /// Shut the write side down after a fully flushed response, then read
    /// what the client still sends until [`Self::drain_linger`] closes.
    fn start_linger(&mut self, deadline: Instant, reason: &'static str) -> MuxResult {
        debug_assert!(
            self.position.is_server() && !self.socket.socket_wants_write(),
            "a linger starts on a frontend with nothing left to flush"
        );
        // The FIN follows the response; the client reads it all, then EOF.
        if let Err(e) = super::shutdown_write(self.socket.socket_ref(), false) {
            debug!(
                "{} H1 closing without lingering after {}: shutdown failed: {:?}",
                log_context!(self),
                reason,
                e
            );
            return MuxResult::CloseSession;
        }
        debug!(
            "{} H1 lingering after {}: draining the rest of the request",
            log_context!(self),
            reason
        );
        self.linger = Linger::Draining {
            deadline,
            remaining: LINGER_MAX_BYTES,
        };
        // Fixed, never re-armed: the deadline bounds the whole drain.
        self.timeout_deadline = Some(deadline);
        self.readiness.interest = Ready::READABLE | Ready::HUP | Ready::ERROR;
        self.readiness.event.remove(Ready::WRITABLE);
        // Edge-triggered epoll will not report bytes already queued.
        self.readiness.event.insert(Ready::READABLE);
        MuxResult::Continue
    }

    /// Read and drop what the client still sends while lingering. Closes on
    /// the client's EOF, a socket error, the byte budget, or the deadline;
    /// otherwise waits for the next READABLE.
    fn drain_linger(&mut self, now: Instant) -> MuxResult {
        let Linger::Draining {
            deadline,
            mut remaining,
        } = self.linger
        else {
            unreachable!("drain_linger runs only while draining");
        };
        if now >= deadline {
            debug!(
                "{} H1 lingering close: deadline reached",
                log_context!(self)
            );
            return MuxResult::CloseSession;
        }
        let position = &self.position;
        let result = match drain_discard(self.socket.socket_ref(), &mut remaining, |size| {
            // Bytes the client sent: counted like any frontend read.
            crate::protocol::mux::h2::record_metric(position.bytes_in_event(size));
        }) {
            LingerRead::Wait => {
                self.readiness.event.remove(Ready::READABLE);
                MuxResult::Continue
            }
            LingerRead::Close => {
                debug!(
                    "{} H1 lingering close: EOF, error or {} bytes left of the budget",
                    log_context!(self),
                    remaining
                );
                MuxResult::CloseSession
            }
        };
        self.linger = Linger::Draining {
            deadline,
            remaining,
        };
        result
    }

    /// Drop the trailer block of a Content-Length-framed message before it
    /// is written to this H1 peer, and return whether one was dropped.
    ///
    /// HTTP/1.1 carries a trailer section only with chunked coding (RFC 9112
    /// §7.1) and a length-framed message ends with its last body byte
    /// (§6.3), while `pkawa::handle_trailer` queues an H2 trailer block for
    /// every peer, so an H2 peer still receives it. A trailer block is the
    /// run of `Header` blocks before a closing `Flags { end_header,
    /// end_stream }` that does not follow the `StatusLine` or `Cookies` of a
    /// header section. Its fields are removed and its closing flags lose
    /// `end_header`, so kawa's H1 serializer writes nothing after the body
    /// (RFC 9110 §6.5.1 lets a recipient discard trailers). An H2 trailer
    /// block is queued whole, and kawa's H1 serializer drains every queued
    /// block in one `prepare`, so the block is never split across passes.
    fn drop_length_framed_trailers(kawa: &mut super::GenericHttpStream) -> bool {
        if !matches!(kawa.body_size, kawa::BodySize::Length(_)) {
            return false;
        }
        let Some(kawa::Block::Flags(kawa::Flags {
            end_header: true,
            end_stream: true,
            ..
        })) = kawa.blocks.back()
        else {
            return false;
        };
        let closing = kawa.blocks.len() - 1;
        let first_field = kawa
            .blocks
            .range(..closing)
            .rposition(|block| !matches!(block, kawa::Block::Header(_)))
            .map_or(0, |before| before + 1);
        if first_field > 0
            && matches!(
                kawa.blocks[first_field - 1],
                kawa::Block::StatusLine | kawa::Block::Cookies
            )
        {
            return false;
        }
        kawa.blocks.drain(first_field..closing);
        if let Some(kawa::Block::Flags(flags)) = kawa.blocks.back_mut() {
            flags.end_header = false;
        }
        warn!(
            "{} trailers of a Content-Length-framed message dropped towards an H1 peer \
             (RFC 9112 §7.1); omit Content-Length to have them forwarded",
            log_module_context!()
        );
        incr!(names::h2::TRAILERS_DROPPED_CONTENT_LENGTH);
        true
    }

    /// Whether the response on this stream has no body by definition: a
    /// response to HEAD (RFC 9110 §9.3.2) or a 204 or 304 (RFC 9110 §15.3.5,
    /// §15.4.5). Its H1 message ends with the header section whatever its
    /// framing fields say (RFC 9112 §6.3 rule 1).
    fn response_has_no_body(context: &crate::protocol::kawa_h1::editor::HttpContext) -> bool {
        context.method == Some(crate::protocol::kawa_h1::parser::Method::Head)
            || matches!(context.status, Some(204 | 304))
    }

    /// Strip the end-of-body framing and the trailer block of a response
    /// that has no body by definition (`response_has_no_body`) before it is
    /// written to this H1 client, and return whether a trailer block was
    /// dropped.
    ///
    /// An H1 client reads such a response as ending with its header section
    /// (RFC 9112 §6.3 rule 1), so it carries neither a last chunk nor a
    /// trailer section, and any byte written after the head is read as the
    /// start of the next response on a keep-alive connection. The end of an
    /// H2 backend stream, an empty DATA frame or a trailer HEADERS frame,
    /// queues `Flags` that kawa's H1 serializer writes as `0\r\n` under
    /// chunked framing, and as the trailer fields and an empty line;
    /// `pkawa::handle_header` gives such a response no chunked framing, and
    /// leaves it open until that end. Every block after the header section
    /// is cleared here: its trailer fields are removed (RFC 9110
    /// §6.5.1 lets a recipient discard trailers) and its `Flags` lose
    /// `end_body`, `end_chunk` and `end_header`, so for a stream ended by a
    /// trailer HEADERS frame or an empty DATA frame nothing is written after
    /// the head. DATA carrying a payload on such a response never reaches
    /// this queue: `ConnectionH2::handle_data_frame` resets the backend
    /// stream of a 204 or a 304 and discards the payload of a response to
    /// HEAD first (RFC 9110 §6.4.1). The header section is the last queued `StatusLine` (an
    /// informational head queued before it is left whole) up to its first
    /// closing `Flags { end_header }`; when it is no longer queued, kawa's
    /// H1 serializer already wrote it whole, since it drains every queued
    /// block in one `prepare`. `drop_length_framed_trailers` runs first and
    /// keeps counting a `Content-Length`-framed trailer block.
    fn drop_bodiless_response_framing(kawa: &mut super::GenericHttpStream) -> bool {
        let after_head = match kawa
            .blocks
            .iter()
            .rposition(|block| matches!(block, kawa::Block::StatusLine))
        {
            Some(status_line) => match kawa.blocks.range(status_line..).position(|block| {
                matches!(
                    block,
                    kawa::Block::Flags(kawa::Flags {
                        end_header: true,
                        ..
                    })
                )
            }) {
                Some(closing) => status_line + closing + 1,
                // The header section is not complete yet.
                None => return false,
            },
            None => 0,
        };
        let mut dropped_trailers = false;
        let mut index = after_head;
        while index < kawa.blocks.len() {
            match &mut kawa.blocks[index] {
                kawa::Block::Header(_) => {
                    kawa.blocks.remove(index);
                    continue;
                }
                kawa::Block::Flags(flags) => {
                    dropped_trailers |= flags.end_header && flags.end_stream;
                    flags.end_body = false;
                    flags.end_chunk = false;
                    flags.end_header = false;
                }
                _ => {}
            }
            index += 1;
        }
        if dropped_trailers {
            warn!(
                "{} trailers of a response without a body dropped towards an H1 client \
                 (RFC 9112 §6.3)",
                log_module_context!()
            );
            incr!(names::h2::TRAILERS_DROPPED_NO_BODY);
        }
        dropped_trailers
    }

    /// End a response body at the backend's EOF.
    ///
    /// Only a body with neither `Content-Length` nor chunked coding
    /// (`BodySize::Empty`) is delimited by the close (RFC 9112 §6.3 rule 8):
    /// it is terminated by pushing END_STREAM flags.
    ///
    /// A `Content-Length` body still expecting bytes, or a chunked body the
    /// backend closed before its terminating `0\r\n\r\n` (RFC 9112 §7.1), is
    /// truncated: RFC 9112 §6.3 rule 5 makes it incomplete. It is demoted to
    /// `ParsingPhase::Error`, so the H2 converter emits
    /// RST_STREAM(InternalError) and the H1 frontend closes the connection,
    /// rather than a silent end of message over a truncated body.
    fn terminate_close_delimited(kawa: &mut super::GenericHttpStream, stream_id: GlobalStreamId) {
        // Pre: we only synthesize an end-of-body for a response still in its
        // body phase. A kawa already Terminated/Error must not be re-terminated
        // (it would double-push an END_STREAM flag onto the converter).
        debug_assert!(
            !kawa.is_terminated(),
            "terminate_close_delimited must not run on an already-terminated kawa"
        );
        debug_assert!(
            kawa.is_main_phase(),
            "terminate_close_delimited must only end a response in its body phase"
        );
        if kawa.body_size != kawa::BodySize::Empty {
            // kawa moves a `Content-Length` body to Terminated once its last
            // byte is parsed, so a `Length` body still in its body phase is
            // missing bytes: `expects` counts them.
            debug_assert!(
                !matches!(kawa.body_size, kawa::BodySize::Length(_)) || kawa.expects > 0,
                "a Content-Length body still in its body phase must be missing bytes"
            );
            warn!(
                "{} H1 backend EOF before the end of a {:?} response on stream {}: emitting RST_STREAM",
                log_module_context!(),
                kawa.body_size,
                stream_id
            );
            incr!(names::h1::BACKEND_EOF_BEFORE_MESSAGE_COMPLETE);
            kawa.parsing_phase
                .error(kawa::ParsingErrorKind::Processing {
                    message: "INTERNAL_ERROR",
                });
            // Post: a truncated body is demoted to Error so the converter
            // emits RST_STREAM, never a silent END_STREAM.
            debug_assert!(
                kawa.is_error(),
                "truncated response must end in the Error phase"
            );
            return;
        }
        debug!(
            "{} H1 close-delimited EOF on stream {}: terminating body",
            log_module_context!(),
            stream_id
        );
        kawa.push_block(kawa::Block::Flags(kawa::Flags {
            end_body: true,
            end_chunk: false,
            end_header: false,
            end_stream: true,
        }));
        kawa.parsing_phase = kawa::ParsingPhase::Terminated;
        // Post: a close-delimited body is now Terminated so the converter
        // emits DATA with END_STREAM on the last frame.
        debug_assert!(
            kawa.is_terminated(),
            "close-delimited body must end in the Terminated phase"
        );
    }

    pub fn readable<E, L>(&mut self, context: &mut Context<L>, mut endpoint: E) -> MuxResult
    where
        E: Endpoint,
        L: ListenerHandler + L7ListenerHandler,
    {
        trace!(
            "{} ======= MUX H1 READABLE {:?}",
            log_context!(self),
            self.position
        );
        if self.is_lingering() {
            // Before `arm_timeout`: the linger deadline is never pushed out.
            return self.drain_linger(context.now);
        }
        let Some(stream_id) = self.stream else {
            error!(
                "{} readable() called on H1 connection with no active stream",
                log_context!(self)
            );
            return MuxResult::Continue;
        };
        self.arm_timeout(context.now);
        let stream = &mut context.streams[stream_id];
        // The answer registry this request captured when it arrived, not the
        // one a reload may have installed since.
        let answers_rc = stream.answers.clone();
        if stream.metrics.start.is_none() {
            stream.metrics.mark_request_start();
        }
        // Read before `split` borrows the stream: the relink guard below
        // needs it while `parts` is alive.
        let answered = stream.state == StreamState::Unlinked;
        let parts = stream.split(&self.position);
        let kawa = parts.rbuffer;

        // If the buffer has no space, don't attempt a read — socket_read with
        // an empty buffer returns (0, Continue) which is indistinguishable from
        // a real EOF. Remove READABLE from the event so the inner loop doesn't
        // spin; `try_resume_reading` will re-arm it once the peer drains the
        // buffer (edge-triggered epoll won't re-fire for data already in the
        // kernel socket buffer).
        if kawa.storage.available_space() == 0 {
            self.readiness.event.remove(Ready::READABLE);
            self.parked_on_buffer_pressure = true;
            // Pair the park flag with the cleared READABLE event: only
            // `try_resume_reading` may re-arm it once the peer drains space.
            debug_assert!(
                self.parked_on_buffer_pressure && !self.readiness.event.is_readable(),
                "parking on buffer pressure must clear the READABLE event"
            );
            return MuxResult::Continue;
        }

        self.parked_on_buffer_pressure = false;
        // Pre: we never ask the socket to read into a full buffer (that path
        // returned above) — `socket_read` requires a non-empty target slice.
        let space_before = kawa.storage.available_space();
        debug_assert!(
            space_before > 0,
            "socket_read must target a buffer with free space"
        );
        let (size, status) = self.socket.socket_read(kawa.storage.space());
        // A read cannot deliver more bytes than the buffer had room for.
        // `debug_assert!` only — `size` is socket-derived, never trusted to
        // panic, but a violation here is a SocketHandler contract bug.
        debug_assert!(
            size <= space_before,
            "socket_read returned more bytes than the buffer could hold"
        );
        context.debug.push(DebugEvent::StreamEvent(0, size));
        if size > 0 && self.position.is_client() {
            // The upstream answered, so this attempt is past the replay
            // boundary (`Stream::can_replay_on_fresh_upstream`) whatever the
            // bytes turn out to parse as. Drop the captured request now
            // instead of carrying it to the end of the response.
            *parts.retry_buffer = None;
        }
        kawa.storage.fill(size);
        debug_assert_eq!(
            kawa.storage.available_space(),
            space_before - size,
            "fill must consume exactly `size` bytes of free space"
        );
        crate::protocol::mux::h2::record_metric(self.position.bytes_in_event(size));
        self.position.count_bytes_in(parts.metrics, size);
        // A backend read that met the EOF records it as HUP, with or without
        // bytes (sozu-proxy/sozu#1603). `Closed` from a backend socket means
        // `read(2)` returned 0 or the connection was reset: the fact
        // `Ready::from(&mio::event::Event)` (`command/src/ready.rs`) reports
        // as HUP from `is_read_closed()`, and whose edge the kernel has
        // already queued. With HUP recorded, the dead-backend check of
        // `Mux::ready_inner` closes this connection on the pass's next inner
        // iteration, after the bytes read here were parsed below. Since
        // sozu-proxy/sozu#1606 a read stops on a short read, so the last bytes
        // and the EOF no longer come back from one `recv`: the EOF is read by
        // a later `readable`, in the same pass when the edge already carried
        // HUP, one `epoll_wait` round later when the FIN landed after that
        // `epoll_wait` returned (HAProxy's trade, `src/raw_sock.c`).
        //
        // Backends only. A frontend `Closed` can also be a TLS
        // `close_notify` on a connection whose TCP stream is still open
        // (`FrontRustls::socket_read`), which the kernel does not report as
        // read-closed, and a frontend HUP closes the whole session at the
        // top of the next pass (`Mux::ready_inner`).
        if status == SocketResult::Closed && self.position.is_client() {
            self.readiness.event.insert(Ready::HUP);
        }
        // A backend buffer can hold bytes no socket event will announce: a
        // backend that writes an interim 1xx and what follows it (another
        // 1xx, the final response) at once leaves the rest unparsed when the
        // interim is forwarded and cleared (`kawa::Kawa::clear` keeps the
        // storage). The writable arms that clear it re-arm this read with
        // `Readiness::signal_pending_read`, and those bytes are parsed here
        // even though the socket has nothing new. A frontend buffer is left
        // alone: its leftover bytes are pipelined requests, which wait for
        // the response in flight.
        let buffered_response =
            self.position.is_client() && !kawa.storage.unparsed_data().is_empty();
        if update_readiness_after_read(size, status, &mut self.readiness) && !buffered_response {
            // size=0: the socket returned EOF (Closed) or WouldBlock.
            // For a close-delimited backend response (no Content-Length, no
            // chunked), a graceful EOF IS the end-of-body signal. Terminate
            // the kawa so the H2 converter emits DATA with END_STREAM.
            // SocketResult::Error (ECONNRESET etc.) is NOT treated as a valid
            // close-delimiter — transport errors should produce 502, not a
            // truncated response.
            if status == SocketResult::Closed
                && self.position.is_client()
                && kawa.is_main_phase()
                && !kawa.is_terminated()
                && !parts.context.keep_alive_backend
            {
                Self::terminate_close_delimited(kawa, stream_id);
                self.timeout_deadline = None;
                self.readiness.interest.remove(Ready::READABLE);
                if let StreamState::Linked(token) = stream.state {
                    // Signal pending write alongside the WRITABLE interest flip:
                    // edge-triggered epoll won't re-fire for bytes we just queued
                    // onto the peer — the synthetic event is the only wake path.
                    let peer = endpoint.readiness_mut(token);
                    peer.arm_writable();
                }
            }
            return MuxResult::Continue;
        }

        let was_main_phase = kawa.is_main_phase();
        let blocks_before_parse = kawa.blocks.len();
        kawa::h1::parse(kawa, parts.context);
        // kawa has no trailer callback: drop spoofed forwarding fields and
        // the fields RFC 9110 §6.5.1 keeps out of trailers from a chunked
        // request's trailer section here, and reject one with too many fields
        // (sozu-proxy/sozu#1689, sozu-proxy/sozu#1701). A trailer section over
        // the limit marks `kawa` in error, which the branch below answers. A
        // backend response is a `Kind::Response` and is left untouched.
        parts
            .context
            .filter_request_trailers(kawa, blocks_before_parse);
        if kawa.is_error() {
            match self.position {
                Position::Client(..) => {
                    incr!(names::http::BACKEND_PARSE_ERRORS);
                    let StreamState::Linked(token) = stream.state else {
                        error!(
                            "{} client stream in error is not in Linked state",
                            log_context!(self)
                        );
                        return MuxResult::CloseSession;
                    };
                    let global_stream_id = stream_id;
                    self.end_stream(global_stream_id, context);
                    endpoint.end_stream(token, global_stream_id, context);
                }
                Position::Server if stream.back.consumed => {
                    // The response already started on the wire — a backend
                    // may answer early and stream its body while the client
                    // is still uploading. A 400 now would land inside that
                    // body as a second, complete response, which the client
                    // reads as part of the first. The response is cut
                    // instead: nothing more is written, and the connection
                    // closes for writing, so the client sees a truncated
                    // message (RFC 9112 §8), never a spliced one.
                    incr!(names::http::FRONTEND_PARSE_ERRORS);
                    warn!(
                        "{} H1 request rejected after its response started: closing",
                        log_context!(self)
                    );
                    // The backend holds part of the rejected request: end its
                    // stream so an H1 connection closes, never pooled even
                    // when its response is complete (sozu-proxy/sozu#1716).
                    // The client connection closes below whatever this flag
                    // says, so clearing it only reaches the backend.
                    if let StreamState::Linked(token) = stream.state {
                        stream.context.keep_alive_backend = false;
                        endpoint.end_stream(token, stream_id, context);
                    }
                    let stream = &mut context.streams[stream_id];
                    forcefully_terminate_answer(
                        stream,
                        &mut self.readiness,
                        H2Error::InternalError,
                    );
                    debug_assert!(
                        stream.back.is_error() && stream.back.out.is_empty(),
                        "a response already started is cut, not replaced"
                    );
                    return self.defer_close_for_tls_flush("request-error-mid-response");
                }
                Position::Server => {
                    incr!(names::http::FRONTEND_PARSE_ERRORS);
                    // A request rejected after its stream was linked, such
                    // as a trailer section over `h2_max_header_fields` read
                    // after the head and body were forwarded, left part of
                    // itself on the backend. End the backend stream before
                    // the answer replaces the response, so the connection
                    // closes and leaves the reverse index, instead of
                    // holding a cut request attached to this stream slot
                    // (sozu-proxy/sozu#1716). The response is cleared first
                    // because a complete one would return an H1 connection
                    // to the keep-alive pool; the 400 replaces it anyway. An
                    // H2 backend only resets this stream. The client
                    // connection closes once the answer is flushed: the
                    // request was never received whole (sozu-proxy/sozu#1721).
                    if let StreamState::Linked(token) = stream.state {
                        stream.back.clear();
                        endpoint.end_stream(token, stream_id, context);
                    }
                    let stream = &mut context.streams[stream_id];
                    let answers = answers_rc.borrow();
                    set_default_answer(stream, &mut self.readiness, 400, &answers);
                }
            }
            return MuxResult::Continue;
        }
        // Capture borrow-sensitive values after parsing but before the 1xx block
        // accesses stream.state (which ends the split borrow from `parts`).
        let is_keep_alive_backend = parts.context.keep_alive_backend;
        let is_body_phase_after_parse = kawa.is_main_phase();

        // 1xx informational responses (every 1xx, RFC 9110 §15.2): the H1
        // parser treats them as complete (Terminated + end_stream=true), but for
        // H2 frontends they must be forwarded WITHOUT END_STREAM so the real
        // response can follow on the same stream. Also keep READABLE interest
        // so the backend can send the final response.
        let is_1xx_backend = if self.position.is_client() {
            if let kawa::StatusLine::Response { code, .. } = &kawa.detached.status_line {
                if (100..200).contains(code) {
                    debug!(
                        "{} H1 backend: received {} informational response",
                        log_context!(self),
                        code
                    );
                    for block in &mut kawa.blocks {
                        if let kawa::Block::Flags(flags) = block {
                            flags.end_stream = false;
                            flags.end_body = false;
                        }
                    }
                    true
                } else {
                    false
                }
            } else {
                false
            }
        } else {
            false
        };
        if kawa.is_terminated() && !is_1xx_backend {
            self.timeout_deadline = None;
            self.readiness.interest.remove(Ready::READABLE);
        }
        if kawa.is_main_phase() {
            if !was_main_phase && self.position.is_client() {
                // The backend's response headers have just finished parsing:
                // the client twin of the `is_server()` branch below, on the
                // same header -> body edge. This is the whole of
                // `backend_header_time`'s H1 arming — nginx's
                // `$upstream_header_time` (sozu-proxy/sozu#426).
                //
                // Unconditional, including on a 1xx: every interim response
                // (100-Continue, 102-199) clears the back buffer in
                // `ConnectionH1::writable`, so the FINAL response re-enters
                // this edge and overwrites, which is the response
                // `SessionMetrics::backend_stop` also anchors on. A 101 has no
                // successor and correctly keeps its own instant.
                parts.metrics.backend_headers_received();
            }
            // A request already answered is never linked. `Mux::timeout_inner`
            // (`lib/src/protocol/mux/mod.rs`) answers 408 on an `Idle` stream
            // whose head was incomplete and leaves it `Unlinked`; the rest of
            // that head may still arrive, and is drained by the linger after
            // the answer (`ConnectionH1::start_linger`), never forwarded.
            if !was_main_phase && self.position.is_server() && !answered {
                if parts.context.method.is_none()
                    || parts.context.authority.is_none()
                    || parts.context.path.is_none()
                {
                    if let kawa::StatusLine::Request {
                        version: kawa::Version::V10,
                        ..
                    } = kawa.detached.status_line
                    {
                        error!(
                            "{} Unexpected malformed request: HTTP/1.0 from {:?} with {:?} {:?} {:?}",
                            log_context!(self),
                            parts.context.session_address,
                            parts.context.method,
                            parts.context.authority,
                            parts.context.path
                        );
                    } else {
                        error!("{} Unexpected malformed request", log_context!(self));
                        kawa::debug_kawa(kawa);
                    }
                    let answers = answers_rc.borrow();
                    set_default_answer(stream, &mut self.readiness, 400, &answers);
                    return MuxResult::Continue;
                }
                // First-seen request on this (server) connection: the keep-alive
                // request counter advances by exactly one and the matching
                // `http.active_requests` gauge `+1` is paired with flipping
                // `request_counted` true (the `generate_access_log` `-1` is
                // gated on that flag, so they must stay balanced).
                let requests_before = self.requests;
                let links_before = context.pending_links.len();
                self.requests += 1;
                debug_assert_eq!(
                    self.requests,
                    requests_before + 1,
                    "server keep-alive request counter must advance by exactly one"
                );
                trace!("{} REQUESTS: {}", log_context!(self), self.requests);
                incr!(names::http::REQUESTS);
                gauge_add!(names::http::ACTIVE_REQUESTS, 1);
                parts.metrics.service_start();
                // Set request_counted after the last use of `parts` to satisfy the borrow checker
                stream.request_counted = true;
                stream.state = StreamState::Link;
                // An H1 frontend has one request in flight, so its session
                // never queues more than one link: size the queue for exactly
                // that on first use instead of four (#1610).
                if context.pending_links.capacity() == 0 {
                    context.pending_links.reserve_exact(1);
                }
                context.pending_links.push_back(stream_id);
                // Post: the stream is queued for backend linking exactly once
                // and is now in the Link state the ready loop expects.
                debug_assert!(
                    stream.request_counted,
                    "request_counted must be set when the active-requests gauge is incremented"
                );
                debug_assert_eq!(
                    stream.state,
                    StreamState::Link,
                    "a first-seen request must transition the stream to Link"
                );
                debug_assert_eq!(
                    context.pending_links.len(),
                    links_before + 1,
                    "a first-seen request must enqueue exactly one pending link"
                );
            }
            if let StreamState::Linked(token) = stream.state {
                // Signal pending write alongside the WRITABLE interest flip: the
                // bytes we just parsed live in sozu's buffers, not the kernel,
                // so edge-triggered epoll won't re-fire on its own.
                let peer = endpoint.readiness_mut(token);
                peer.arm_writable();
            }
        };
        // 1xx informational: kawa sets `detached.status_line` as soon as the
        // status line parses and only then moves to `ParsingPhase::Headers`,
        // so a read that stops inside the 1xx headers leaves
        // `is_main_phase()` false while `is_1xx_backend` is already true. On
        // that pass only this arm wakes the frontend, early, before the 1xx is
        // complete; the pass that completes the headers re-arms it through the
        // `Linked` arm above either way. When the whole 1xx arrives in one
        // read it parses to `Terminated`, which kawa counts as a main phase, so
        // the `Linked` arm above has already fired and this call is redundant
        // (`arm_writable` is idempotent).
        if is_1xx_backend && let StreamState::Linked(token) = stream.state {
            let peer = endpoint.readiness_mut(token);
            peer.arm_writable();
        }

        // Close-delimited response: socket_read returned (size > 0, Closed) —
        // the last data chunk arrived together with the EOF in a single read.
        // After parsing the data above, terminate the kawa now so the H2
        // converter emits END_STREAM on the last DATA frame.
        if status == SocketResult::Closed
            && self.position.is_client()
            && is_body_phase_after_parse
            && !is_keep_alive_backend
            && !context.streams[stream_id].back.is_terminated()
        {
            let kawa = &mut context.streams[stream_id].back;
            Self::terminate_close_delimited(kawa, stream_id);
            self.timeout_deadline = None;
            self.readiness.interest.remove(Ready::READABLE);
        }

        MuxResult::Continue
    }

    pub fn writable<E, L>(&mut self, context: &mut Context<L>, mut endpoint: E) -> MuxResult
    where
        E: Endpoint,
        L: ListenerHandler + L7ListenerHandler,
    {
        trace!(
            "{} ======= MUX H1 WRITABLE {:?}",
            log_context!(self),
            self.position
        );
        if self.is_lingering() {
            // The write side is shut down: nothing is left to write, and the
            // linger deadline is never pushed out.
            self.readiness.interest.remove(Ready::WRITABLE);
            self.readiness.event.remove(Ready::WRITABLE);
            return MuxResult::Continue;
        }
        let Some(stream_id) = self.stream else {
            if self.socket.socket_wants_write() {
                let (size, status) = self.socket.socket_write_vectored(&[]);
                let _ = update_readiness_after_write(size, status, &mut self.readiness);
                if let Some(result) = self.client_write_failed(status) {
                    return result;
                }
                // Only after `Continue`: see the same check below.
                if self.socket.socket_wants_write() && status == SocketResult::Continue {
                    self.readiness.signal_pending_write();
                }
            }
            return MuxResult::Continue;
        };
        // One clock read for the whole pass: the two later re-arms below
        // (100-Continue, keep-alive recycle) run while `context` is mutably
        // reborrowed for the stream, so they cannot reach `context.now`.
        let now = context.now;
        self.arm_timeout(now);
        let stream = &mut context.streams[stream_id];
        let parts = stream.split(&self.position);
        let kawa = parts.wbuffer;
        // Apply per-frontend response-side header edits stashed by the
        // routing layer at request time. Only the Server-position pass
        // touches the response back-kawa; the Client-position pass
        // (writing the request to the backend) has already had its
        // edits applied in `Router::route_from_request`.
        //
        // Drained via `mem::take` so the injection runs exactly once
        // per response. H1 keep-alive can re-enter this writable path
        // for the same stream when the backend response spans more
        // than one TCP read; without the take, the second pass would
        // re-insert the same headers (typically as duplicate STS lines
        // on the wire — RFC 6797 §6.1 expects a single header). On H2
        // the same multi-prepare-cycle pattern surfaces as a
        // `H2BlockConverter::finalize` "out buffer not empty" leak.
        if matches!(self.position, Position::Server) && !parts.context.headers_response.is_empty() {
            let edits = std::mem::take(&mut parts.context.headers_response);
            super::shared::apply_response_header_edits(kawa, &edits);
        }
        Self::drop_length_framed_trailers(kawa);
        if matches!(self.position, Position::Server) && Self::response_has_no_body(parts.context) {
            Self::drop_bodiless_response_framing(kawa);
        }
        kawa.prepare(&mut kawa::h1::BlockConverter);
        // SAFETY: the descriptors `gather` pushes borrow memory `kawa` owns:
        // a `Store::Slice` or `Detached` points into `kawa.storage`, while an
        // `Alloc`, `Static` or `Shared` store points at bytes held by the
        // block in `kawa.out` itself. So nothing may mutate `kawa`, neither
        // `storage` nor `out`, while they are live: no `push_out`, no
        // `storage.fill`, no `consume` and no `clear` between this call and
        // the `h2_transmit::confirm` below, which clears `self.io_slices`
        // before its own `Kawa::consume`. In between, the pass only writes
        // the descriptors to the socket, which reborrows them for the call
        // and cannot retain them, copies them into the replay capture, which
        // mutates `retry_buffer` and never `kawa`, and pushes a debug event
        // onto `context.debug`. Its one `return` in that window is taken only
        // when the vector is empty, so no descriptor outlives the pass.
        let queued = unsafe { super::h2_transmit::gather(kawa, &mut self.io_slices) };
        // A response in the Error phase ends here too: the backend truncated
        // it or it was forcefully terminated, so once what was queued before
        // the failure is flushed the connection closes (see below).
        let can_finalize_server_close = matches!(self.position, Position::Server)
            && (kawa.is_terminated() || kawa.is_error())
            && kawa.is_completed();
        if self.io_slices.is_empty()
            && !self.socket.socket_wants_write()
            && !can_finalize_server_close
        {
            self.readiness.interest.remove(Ready::WRITABLE);
            return MuxResult::Continue;
        }
        let tls_only_flush = self.io_slices.is_empty();
        // `queued` is the total the gathered slices offer the socket; a
        // vectored write can never report more consumed than we handed it.
        let (size, status) = self.socket.socket_write_vectored(&self.io_slices);
        debug_assert!(
            size <= queued,
            "socket_write_vectored reported more bytes written than were queued"
        );
        // Capture the bytes the socket accepted BEFORE `kawa::Kawa::consume`
        // drops them from the front buffer and shifts the storage: past that
        // point sozu no longer holds the request, so a stale pooled upstream
        // could not be retried without this copy. Every proxy that retries a
        // request already written upstream pays the same copy — HAProxy
        // ("requires to allocate a buffer and copy the whole request into
        // it, so it has memory and performance impacts"), pingora
        // (`enable_retry_buffering`, a fixed 64 KiB `FixedBuffer`).
        //
        // It is charged ONLY to a connection that came out of the keep-alive
        // pool: `reused_from_pool` is set by `start_stream` on the
        // KeepAlive -> Connected transition and nowhere else, so a freshly
        // dialled upstream and every `Position::Server` write cost nothing.
        // That is pingora's `RetryType::ReusedOnly` boundary, argued in
        // `Stream::can_replay_on_fresh_upstream` (sozu-proxy/sozu#1442).
        //
        // Overflowing one front buffer truncates the capture to `None` and
        // the request stops being replayable, rather than the buffer
        // growing: HAProxy, same reason — "Requests not fitting in a single
        // buffer will never be retried".
        //
        // The bytes are appended straight onto the capture, with the budget
        // decided first: `size` is already known, `kawa` was moved out of
        // `parts` above so `parts.retry_buffer` is independently borrowable,
        // and the slices still alias `kawa.storage` until `consume` below.
        // Staging them through a temporary `Vec` first would cost one extra
        // allocation, one extra copy and one free on every write of every
        // pooled connection.
        let retry_budget = kawa.storage.capacity();
        if self.reused_from_pool {
            match parts.retry_buffer.as_mut() {
                // The capture has to stay a byte-exact prefix of what the
                // socket accepted, so a write that would carry it past one
                // front buffer drops it whole rather than truncating it.
                Some(buffer) if size <= retry_budget.saturating_sub(buffer.len()) => {
                    buffer.reserve(size);
                    let mut remaining = size;
                    for slice in &self.io_slices {
                        if remaining == 0 {
                            break;
                        }
                        let take = remaining.min(slice.len());
                        buffer.extend_from_slice(&slice[..take]);
                        remaining -= take;
                    }
                    debug_assert_eq!(
                        remaining, 0,
                        "the replay capture must mirror every byte the socket accepted"
                    );
                }
                _ => *parts.retry_buffer = None,
            }
        }
        context.debug.push(DebugEvent::StreamEvent(1, size));
        super::h2_transmit::confirm(kawa, &mut self.io_slices, size);
        crate::protocol::mux::h2::record_metric(self.position.bytes_out_event(size));
        self.position.count_bytes_out(parts.metrics, size);
        if self.position.is_server() {
            context.client_bytes_out = context.client_bytes_out.wrapping_add(size);
        }
        let should_yield = update_readiness_after_write(size, status, &mut self.readiness);
        if let Some(result) = self.client_write_failed(status) {
            return result;
        }
        if self.socket.socket_wants_write() {
            // Only a write that answered `Continue` queues a synthetic
            // event. A socket that answered `WouldBlock` is full: the kernel
            // raises the next WRITABLE edge once the peer reads. Re-raising
            // it here while rustls still held the records the kernel refused
            // made `Mux::ready_inner` call this write again on every inner
            // iteration, each answering `WouldBlock`, until
            // `MAX_LOOP_ITERATIONS` counted `http.infinite_loop.error`
            // (sozu-proxy/sozu#1780). An `Error` or `Closed` write has nothing
            // to retry either: on the frontend `client_write_failed` closed the
            // session above; on a backend the hang-up that follows closes it.
            if status == SocketResult::Continue {
                self.readiness.signal_pending_write();
                // Pair the queued-write signal with the socket's own report: we
                // only synthesize a WRITABLE event when the socket still has bytes
                // buffered (edge-triggered epoll won't re-fire on its own).
                debug_assert!(
                    self.readiness.event.is_writable(),
                    "signal_pending_write must leave a WRITABLE event queued"
                );
            } else {
                debug_assert!(
                    !self.readiness.event.is_writable(),
                    "a write that did not answer Continue must leave WRITABLE to the next edge"
                );
            }
            return MuxResult::Continue;
        }
        if !tls_only_flush && should_yield {
            return MuxResult::Continue;
        }

        if matches!(self.position, Position::Server) && kawa.is_error() && kawa.is_completed() {
            // The response failed after part of it may have left: a body the
            // backend truncated (`ConnectionH1::terminate_close_delimited`) or
            // a response `forcefully_terminate_answer` ended. H1 has no
            // RST_STREAM, and neither a default answer nor the next pipelined
            // response may follow those bytes on this connection, so close
            // it: the client sees a close before the end of the message, which
            // RFC 9112 §6.3 rule 5 makes the incomplete-message signal. Never
            // kept alive; `Mux::close` logs the stream as an error.
            debug_assert!(
                !kawa.is_terminated(),
                "an errored response must not also be terminated"
            );
            debug!(
                "{} H1 closing the frontend after an incomplete response on stream {}",
                log_context!(self),
                stream_id
            );
            return self.defer_close_for_tls_flush("incomplete-response");
        }
        if kawa.is_terminated() && kawa.is_completed() {
            match self.position {
                Position::Client(..) => self.readiness.interest.insert(Ready::READABLE),
                Position::Server => {
                    if stream.context.closing {
                        return self.defer_close_for_tls_flush("closing-context");
                    }
                    let kawa = &mut stream.back;
                    match kawa.detached.status_line {
                        kawa::StatusLine::Response { code: 101, .. } => {
                            debug!("{} ============== HANDLE UPGRADE!", log_context!(self));
                            stream.metrics.backend_stop();
                            let client_rtt = self.rtt();
                            let server_rtt =
                                stream.linked_token().and_then(|t| endpoint.peer_rtt(t));
                            for event in stream
                                .generate_access_log(
                                    false,
                                    Some("H1::Upgrade"),
                                    context.listener.clone(),
                                    client_rtt,
                                    server_rtt,
                                )
                                .into_iter()
                                .flatten()
                            {
                                crate::protocol::mux::h2::record_metric(event);
                            }
                            return MuxResult::Upgrade;
                        }
                        kawa::StatusLine::Response { code: 100, .. } => {
                            debug!("{} ============== HANDLE CONTINUE!", log_context!(self));
                            // After a 100 Continue, we expect the client to continue
                            // with its request body. Do NOT call generate_access_log
                            // here — the final response will emit the access log.
                            // Calling it here would double-decrement http.active_requests.
                            self.timeout_deadline = now.checked_add(self.timeout_duration);
                            self.readiness.interest.insert(Ready::READABLE);
                            kawa.clear();
                            stream.metrics.backend_stop();
                            if let StreamState::Linked(token) = stream.state {
                                // The final response may already sit in the
                                // backend buffer, read with the 100: no
                                // socket event will announce it, so the read
                                // is re-armed, not only its interest.
                                let backend = endpoint.readiness_mut(token);
                                backend.interest.insert(Ready::READABLE);
                                backend.signal_pending_read();
                            }
                            return MuxResult::Continue;
                        }
                        // Every other 1xx (102 Processing, 103 Early Hints and
                        // the unassigned 104-199) is interim too: RFC 9110
                        // §15.2 has a client accept any number of them before
                        // the final response, so the stream stays linked and
                        // the backend stays out of the keep-alive pool until
                        // that final response has been written.
                        kawa::StatusLine::Response { code, .. } if (102..200).contains(&code) => {
                            debug!(
                                "{} ============== HANDLE INTERIM {}!",
                                log_context!(self),
                                code
                            );
                            // Do NOT call generate_access_log for an interim response.
                            // The final response will emit the access log.
                            // Calling it here would double-decrement http.active_requests.
                            if let StreamState::Linked(token) = stream.state {
                                // after an interim response, we expect the backend to send its final
                                // response, which may already sit in the backend buffer, read with
                                // the interim: no socket event will announce it, so the read is
                                // re-armed, not only its interest.
                                let backend = endpoint.readiness_mut(token);
                                backend.interest.insert(Ready::READABLE);
                                backend.signal_pending_read();
                                kawa.clear();
                                stream.metrics.backend_stop();
                                return MuxResult::Continue;
                            } else {
                                stream.metrics.backend_stop();
                                let client_rtt = self.rtt();
                                let server_rtt =
                                    stream.linked_token().and_then(|t| endpoint.peer_rtt(t));
                                for event in stream
                                    .generate_access_log(
                                        false,
                                        Some("H1::Interim"),
                                        context.listener.clone(),
                                        client_rtt,
                                        server_rtt,
                                    )
                                    .into_iter()
                                    .flatten()
                                {
                                    crate::protocol::mux::h2::record_metric(event);
                                }
                                return self.defer_close_for_tls_flush("interim");
                            }
                        }
                        _ => {}
                    }
                    incr!(names::http::E2E_HTTP11);
                    stream.metrics.backend_stop();
                    let client_rtt = self.rtt();
                    let server_rtt = stream.linked_token().and_then(|t| endpoint.peer_rtt(t));
                    for event in stream
                        .generate_access_log(
                            false,
                            Some("H1::Complete"),
                            context.listener.clone(),
                            client_rtt,
                            server_rtt,
                        )
                        .into_iter()
                        .flatten()
                    {
                        crate::protocol::mux::h2::record_metric(event);
                    }
                    stream.metrics.reset();
                    let old_state = std::mem::replace(&mut stream.state, StreamState::Unlinked);
                    if let StreamState::Linked(token) = old_state {
                        remove_backend_stream(&mut context.backend_streams, token, stream_id);
                    }
                    // The connection outlives the response only when neither
                    // side asked to close it. `keep_alive_backend` is false
                    // once the backend's response carried `Connection: close`,
                    // or was a non-persistent HTTP/1.0 one that sozu forwards
                    // as HTTP/1.1 with `Connection: close` added
                    // (`HttpContext::on_response_headers`,
                    // `lib/src/protocol/kawa_h1/editor.rs`): sozu forwards that
                    // header, so RFC 9112 §9.6 requires it to close after this
                    // response and to process no further request on it. It
                    // is also the only way a close-delimited body ends here:
                    // `ConnectionH1::terminate_close_delimited` runs only for
                    // such a backend, and that body ends, for the client too,
                    // only with the close (§6.3 rule 8). Keeping the
                    // connection left the client waiting for more body until
                    // the frontend timeout, and appended the next pipelined
                    // response to the body (sozu-proxy/sozu#1642).
                    //
                    // The request must have been received whole as well: a
                    // backend, or a default answer, may answer early (a 413 or
                    // a 401 on an upload) while the client is still sending
                    // the body. RFC 9112 §9.3 lets a connection carry another
                    // message only once the previous one is complete, and the
                    // rest of that body belongs to the answered request.
                    // Resetting the parser here would read it as a new
                    // request (sozu-proxy/sozu#1721), so the connection closes
                    // instead. Whether the request was also fully written to
                    // the backend decides only whether that backend may be
                    // pooled: `ConnectionH1::end_stream` checks it, and a
                    // default answer leaves a request it never forwarded.
                    //
                    // Pre: the decision is taken once, when the whole response
                    // has left, never while part of it is still queued.
                    debug_assert!(
                        stream.back.is_terminated() && stream.back.is_completed(),
                        "the keep-alive decision must follow a completely written response"
                    );
                    let request_complete = stream.front.is_terminated();
                    let keep_alive = request_complete
                        && stream.context.keep_alive_frontend
                        && stream.context.keep_alive_backend;
                    if keep_alive {
                        self.timeout_deadline = now.checked_add(self.timeout_duration);
                        if let StreamState::Linked(token) = old_state {
                            endpoint.end_stream(token, stream_id, context);
                        }
                        self.readiness.interest.insert(Ready::READABLE);
                        // The next request on this connection is a new
                        // request: it gets its own id, minted from the same
                        // per-session source as an H2 stream's.
                        let request_id = context.next_request_id();
                        let stream = &mut context.streams[stream_id];
                        stream.context.reset(request_id);
                        stream.back.clear();
                        stream.back.storage.clear();
                        stream.front.clear();
                        // do not stream.front.storage.clear() because of H1 pipelining
                        // The next request has not been encoded for any
                        // backend yet (sozu-proxy/sozu#1632).
                        stream.front_bound_to_backend = false;
                        stream.attempts = 0;
                        stream.tried_backends.clear();
                        // The next request is a new H2 stream on an H2
                        // backend connection, which reads these to refuse a
                        // frame on a closed stream (RFC 9113 §5.1) and to
                        // check `content-length` (§8.1.1): clear them as
                        // `Context::create_stream` does for a recycled slot,
                        // or the backend's response HEADERS is refused as
                        // arriving after END_STREAM (sozu-proxy/sozu#1781).
                        stream.front_received_end_of_stream = false;
                        stream.back_received_end_of_stream = false;
                        stream.front_data_received = 0;
                        stream.back_data_received = 0;
                        // The next pipelined request gets its own replay
                        // decision: `start_stream` re-arms capture only if it
                        // again picks a connection out of the keep-alive pool.
                        stream.forget_upstream_replay();
                        // Transition back to Idle so buffered pipelined requests
                        // trigger a phase transition on the next readable() call.
                        stream.state = StreamState::Idle;
                        // Post: the keep-alive reset leaves a clean, closed slot
                        // ready for the next pipelined request. `request_counted`
                        // was already cleared by `generate_access_log` above, so
                        // the slot carries no pending active-requests charge.
                        debug_assert_eq!(
                            stream.state,
                            StreamState::Idle,
                            "keep-alive reset must return the stream to Idle"
                        );
                        debug_assert_eq!(stream.attempts, 0, "keep-alive reset must zero attempts");
                        debug_assert!(
                            !stream.request_counted,
                            "keep-alive reset must leave no counted request (active-requests leak)"
                        );
                        debug_assert!(
                            stream.back.storage.is_empty(),
                            "keep-alive reset must drain the response storage"
                        );
                        // HTTP/1.1 pipelining: if there's still data in the frontend
                        // storage (pipelined requests already read from the socket),
                        // parse it now. We can't rely on a new READABLE event because
                        // the socket buffer may be empty — all requests were already
                        // read into kawa storage in the first socket_read.
                        if !stream.front.storage.is_empty() {
                            let blocks_before_parse = stream.front.blocks.len();
                            kawa::h1::parse(&mut stream.front, &mut stream.context);
                            stream
                                .context
                                .filter_request_trailers(&mut stream.front, blocks_before_parse);
                            let is_error = stream.front.is_error();
                            let is_main = stream.front.is_main_phase();
                            let malformed = is_main
                                && (stream.context.method.is_none()
                                    || stream.context.authority.is_none()
                                    || stream.context.path.is_none());
                            if is_error || malformed {
                                let answers_rc = stream.answers.clone();
                                let answers = answers_rc.borrow();
                                set_default_answer(stream, &mut self.readiness, 400, &answers);
                            } else if is_main {
                                self.requests += 1;
                                incr!(names::http::REQUESTS);
                                gauge_add!(names::http::ACTIVE_REQUESTS, 1);
                                stream.metrics.service_start();
                                stream.request_counted = true;
                                stream.state = StreamState::Link;
                                context.pending_links.push_back(stream_id);
                            }
                            // else: incomplete parse, wait for more data via READABLE
                        }
                    } else {
                        // Pair: at least one side asked for the close, or
                        // the request was not received whole. A
                        // pipelined request already buffered is dropped with
                        // the connection, never answered on it; the client
                        // retries it on a new connection (RFC 9112 §9.3.2).
                        debug_assert!(
                            !request_complete
                                || !stream.context.keep_alive_frontend
                                || !stream.context.keep_alive_backend,
                            "the frontend closes only when one side asked to or the request is incomplete"
                        );
                        // A TLS close is deferred until `close_notify` is
                        // flushed, and `writable` runs again: mark the stream
                        // closing so that pass takes the `closing-context`
                        // exit above, instead of completing the response a
                        // second time (a second access log and counters) and
                        // re-taking the keep-alive decision after
                        // `close_notify`.
                        stream.context.closing = true;
                        // No byte of the request arrived (a 408 to a silent
                        // client): nothing is in flight, the close cannot
                        // reset anything away, so it does not linger.
                        let request_started =
                            !matches!(stream.front.parsing_phase, kawa::ParsingPhase::StatusLine)
                                || !stream.front.storage.is_empty();
                        if !request_complete && request_started {
                            // The client may still be sending the request:
                            // drain it once the response is flushed, so the
                            // close does not reset the response away (RFC
                            // 9112 §9.6). The backend is done with either way.
                            self.linger = Linger::Pending {
                                deadline: now + self.linger_timeout,
                            };
                            if let StreamState::Linked(token) = old_state {
                                endpoint.end_stream(token, stream_id, context);
                            }
                        }
                        let stream = &mut context.streams[stream_id];
                        if stream.context.keep_alive_frontend && stream.context.keep_alive_backend {
                            // Both sides wanted to keep it: only the
                            // incomplete request closes it.
                            debug_assert!(
                                !request_complete,
                                "with both sides keeping alive, only an incomplete request closes"
                            );
                            debug!(
                                "{} H1 closing the frontend after a response that completed before its request on stream {}",
                                log_context!(self),
                                stream_id
                            );
                            incr!(names::http::CLOSE_REQUEST_INCOMPLETE);
                            return self.defer_close_for_tls_flush("request-incomplete");
                        }
                        if !stream.context.keep_alive_backend {
                            debug!(
                                "{} H1 closing the frontend after a response carrying the backend's Connection: close on stream {}",
                                log_context!(self),
                                stream_id
                            );
                        }
                        return self.defer_close_for_tls_flush("response-complete");
                    }
                }
            }
        }
        MuxResult::Continue
    }

    pub fn force_disconnect(&mut self) -> MuxResult {
        match &mut self.position {
            Position::Client(_, _, status) => {
                *status = BackendStatus::Disconnecting;
                self.readiness.event = Ready::HUP;
                debug!(
                    "{} H1 force_disconnect client: stream={:?}, wants_write={}, readiness={:?}",
                    log_context!(self),
                    self.stream,
                    self.socket.socket_wants_write(),
                    self.readiness
                );
                MuxResult::Continue
            }
            Position::Server => {
                if self.socket.socket_wants_write() {
                    debug!(
                        "{} H1 force_disconnect delaying close: stream={:?}, wants_write=true, readiness={:?}",
                        log_context!(self),
                        self.stream,
                        self.readiness
                    );
                    self.readiness.interest = Ready::WRITABLE | Ready::HUP | Ready::ERROR;
                    self.readiness.signal_pending_write();
                    MuxResult::Continue
                } else {
                    debug!(
                        "{} H1 force_disconnect closing session: stream={:?}, wants_write=false, readiness={:?}",
                        log_context!(self),
                        self.stream,
                        self.readiness
                    );
                    MuxResult::CloseSession
                }
            }
        }
    }

    pub fn has_pending_write(&self) -> bool {
        !self.write_failed && self.socket.socket_wants_write()
    }

    /// A write to the client failed: the client is gone, or its socket can
    /// never be flushed. Close at once; `Self::has_pending_write` stops
    /// reporting output, so no delayed close waits for a flush that cannot
    /// happen.
    fn client_write_failed(&mut self, status: SocketResult) -> Option<MuxResult> {
        if self.position.is_server() && matches!(status, SocketResult::Error | SocketResult::Closed)
        {
            debug!(
                "{} H1 closing the frontend after a failed write: {:?}",
                log_context!(self),
                status
            );
            self.write_failed = true;
            Some(MuxResult::CloseSession)
        } else {
            None
        }
    }

    pub fn initiate_close_notify(&mut self) -> bool {
        if !self.position.is_server() {
            return false;
        }
        // Past the guard we are always server-side; close_notify is a
        // frontend-only TLS concern.
        debug_assert!(
            self.position.is_server(),
            "initiate_close_notify past the guard must be server-side"
        );
        if !self.close_notify_sent {
            trace!("{} H1 initiating CLOSE_NOTIFY", log_context!(self));
            self.socket.socket_close();
            self.close_notify_sent = true;
        }
        // `close_notify` is monotone: once requested it stays sent for the
        // connection's lifetime (a second send would corrupt the TLS stream).
        debug_assert!(
            self.close_notify_sent,
            "close_notify_sent must be set once initiate_close_notify has run"
        );
        if self.socket.socket_wants_write() {
            self.readiness.arm_writable();
            // arm_writable pairs interest + event so the deferred TLS flush is
            // actually scheduled under edge-triggered epoll.
            debug_assert!(
                self.readiness.interest.is_writable() && self.readiness.event.is_writable(),
                "arm_writable must set both WRITABLE interest and event"
            );
            true
        } else {
            false
        }
    }

    pub fn close<E, L>(&mut self, context: &mut Context<L>, mut endpoint: E)
    where
        E: Endpoint,
        L: ListenerHandler + L7ListenerHandler,
    {
        match self.position {
            Position::Client(_, _, BackendStatus::KeepAlive)
            | Position::Client(_, _, BackendStatus::Disconnecting) => {
                trace!("{} close detached client ConnectionH1", log_context!(self));
                return;
            }
            Position::Client(_, _, BackendStatus::Connecting(_))
            | Position::Client(_, _, BackendStatus::Connected) => {
                debug!(
                    "{} BACKEND CLOSING FOR: {:?} {:?}",
                    log_context!(self),
                    self.position,
                    self.stream
                );
            }
            Position::Server => {
                let tls_pending_before = self.socket.socket_wants_write();
                let (tls_pending_after, drain_rounds) =
                    drain_tls_close_notify(&mut self.socket, &mut self.close_notify_sent);
                if tls_pending_after {
                    error!(
                        "{} H1 TLS buffer NOT fully drained on close: pending_before={}, pending_after={}, drain_rounds={}, stream={:?}, close_notify_sent={}, readiness={:?}",
                        log_context!(self),
                        tls_pending_before,
                        tls_pending_after,
                        drain_rounds,
                        self.stream,
                        self.close_notify_sent,
                        self.readiness
                    );
                }
                return;
            }
        }
        let Some(stream_id) = self.stream else {
            trace!(
                "{} closing detached H1 client with no active stream",
                log_context!(self)
            );
            return;
        };
        // reconnection is handled by the server
        let StreamState::Linked(token) = context.streams[stream_id].state else {
            trace!(
                "{} closing detached H1 client in state {:?} on stream {}",
                log_context!(self),
                context.streams[stream_id].state,
                stream_id
            );
            return;
        };
        endpoint.end_stream(token, stream_id, context)
    }

    pub fn end_stream<L>(&mut self, stream: GlobalStreamId, context: &mut Context<L>)
    where
        L: ListenerHandler + L7ListenerHandler,
    {
        if self.stream != Some(stream) {
            error!(
                "{} end_stream called with stream {} but expected {:?}",
                log_context!(self),
                stream,
                self.stream
            );
            return;
        }
        // Reached only on the matched-stream path: the connection's active
        // stream is exactly the one being ended.
        debug_assert_eq!(
            self.stream,
            Some(stream),
            "end_stream past the guard must target the active stream"
        );
        context.unlink_stream(stream);
        // Post: whatever backend token this stream was Linked to no longer
        // lists it in the reverse index. `unlink_stream` is idempotent — it
        // evicts only while the stream is still `Linked` — so a subsequent
        // end/close cannot double-remove it. It is NOT the module's only
        // eviction point: `remove_backend_stream` has direct callers here and
        // in `h2.rs`. Extra eviction is harmless; what this post-condition
        // claims is that THIS path evicted.
        // (The `state` field is still `Linked` here; the arms below retire it.)
        #[cfg(debug_assertions)]
        if let StreamState::Linked(token) = context.streams[stream].state {
            debug_assert!(
                context
                    .backend_streams
                    .get(&token)
                    .is_none_or(|ids| !ids.contains(&stream)),
                "unlink_stream must evict the stream from the backend reverse index"
            );
        }
        let stream_id = stream;
        let stream = &mut context.streams[stream_id];
        let answers_rc = stream.answers.clone();
        let stream_context = &mut stream.context;
        trace!(
            "{} end H1 stream {:?}: {:#?}",
            log_context!(self),
            self.stream,
            stream_context
        );
        match &mut self.position {
            Position::Client(_, _, BackendStatus::Connecting(_)) => {
                self.stream = None;
                if stream.state != StreamState::Recycle {
                    stream.state = StreamState::Unlinked;
                }
                // Post: the client connection detaches from its stream and the
                // slot is retired (Unlinked) unless already marked for reuse —
                // never left dangling in an open/Linked state.
                debug_assert!(
                    self.stream.is_none(),
                    "client end_stream must detach the stream"
                );
                debug_assert!(
                    !matches!(stream.state, StreamState::Linked(_)),
                    "detached stream must not remain Linked"
                );
                self.readiness.interest.remove(Ready::ALL);
                self.force_disconnect();
            }
            Position::Client(_, _, status @ BackendStatus::Connected) => {
                self.stream = None;
                if stream.state != StreamState::Recycle {
                    stream.state = StreamState::Unlinked;
                }
                debug_assert!(
                    self.stream.is_none(),
                    "client end_stream must detach the stream"
                );
                debug_assert!(
                    !matches!(stream.state, StreamState::Linked(_)),
                    "detached stream must not remain Linked"
                );
                self.readiness.interest.remove(Ready::ALL);
                // keep alive should probably be used only if the http context is fully reset
                // in case end_stream occurs due to an error the connection state is probably
                // unrecoverable and should be terminated
                // Both messages must be complete: a backend that answered
                // before the whole request was written to it still expects
                // the rest of the body, and would read the next request
                // pooled onto it as that body (RFC 9112 §9.3,
                // sozu-proxy/sozu#1721).
                if stream_context.keep_alive_backend
                    && stream.back.is_terminated()
                    && stream.front.is_terminated()
                    && stream.front.is_completed()
                {
                    *status = BackendStatus::KeepAlive;
                } else {
                    self.force_disconnect();
                }
            }
            Position::Client(_, _, BackendStatus::KeepAlive)
            | Position::Client(_, _, BackendStatus::Disconnecting) => {
                error!(
                    "{} end_stream called on KeepAlive or Disconnecting H1 client",
                    log_context!(self)
                );
            }
            Position::Server => match end_stream_decision(stream) {
                EndStreamAction::ForwardTerminated => {
                    debug!("{} CLOSING H1 TERMINATED STREAM", log_context!(self));
                    stream.state = StreamState::Unlinked;
                    self.readiness.interest.insert(Ready::WRITABLE);
                    // End-of-stream was already queued into kawa by the parser;
                    // no fresh WRITABLE event will arrive from the kernel.
                    self.readiness.signal_pending_write();
                }
                EndStreamAction::CloseDelimited => {
                    debug!("{} CLOSE DELIMITED", log_context!(self));
                    stream.state = StreamState::Unlinked;
                    self.readiness.arm_writable();
                }
                EndStreamAction::ForwardUnterminated => {
                    debug!("{} CLOSING H1 UNTERMINATED STREAM", log_context!(self));
                    forcefully_terminate_answer(
                        stream,
                        &mut self.readiness,
                        H2Error::InternalError,
                    );
                }
                EndStreamAction::SendDefault(status) => {
                    let answers = answers_rc.borrow();
                    set_default_answer(stream, &mut self.readiness, status, &answers);
                }
                EndStreamAction::Reconnect => {
                    debug!("{} H1 RECONNECT", log_context!(self));
                    stream.state = StreamState::Link;
                    context.pending_links.push_back(stream_id);
                }
                EndStreamAction::ReplayOnFreshBackend => match stream.queue_upstream_replay() {
                    Some(len) => {
                        debug!(
                            "{} H1 REPLAY {} request bytes on a fresh backend",
                            log_context!(self),
                            len
                        );
                        incr!(
                            names::backend::RETRY_STALE_UPSTREAM,
                            stream.context.cluster_id.as_deref(),
                            stream.context.backend_id.as_deref()
                        );
                        stream.state = StreamState::Link;
                        context.pending_links.push_back(stream_id);
                        // `Router::plan_connect` still gates on `stream.attempts`
                        // against `max_connection_attempts`, so a cluster whose backends
                        // are all stale cannot loop: the budget runs out and
                        // the caller answers 503.
                    }
                    None => {
                        // Unreachable while `end_stream_decision` only picks
                        // this action behind `can_replay_on_fresh_upstream`,
                        // which requires the buffer. Degrade to the answer
                        // the old code sent rather than stranding the stream.
                        error!(
                            "{} replay selected with no captured request",
                            log_context!(self)
                        );
                        let answers = answers_rc.borrow();
                        set_default_answer(stream, &mut self.readiness, 502, &answers);
                    }
                },
            },
        }
    }

    pub fn start_stream<L>(&mut self, stream: GlobalStreamId, context: &mut Context<L>) -> bool
    where
        L: ListenerHandler + L7ListenerHandler,
    {
        trace!(
            "{} start H1 stream {} {:?}",
            log_context!(self),
            stream,
            self.readiness
        );
        self.readiness.interest.insert(Ready::ALL);
        self.stream = Some(stream);
        debug_assert_eq!(
            self.stream,
            Some(stream),
            "start_stream must pin the connection's active stream"
        );
        match &mut self.position {
            Position::Client(_, _, status @ BackendStatus::KeepAlive) => {
                *status = BackendStatus::Connected;
                // This stream is being written onto a socket that has been
                // idle in the pool, so the peer may already have closed it
                // without sozu noticing. Record the provenance and arm
                // request capture so the write path below can keep the bytes
                // needed to replay elsewhere (sozu-proxy/sozu#1442).
                self.reused_from_pool = true;
                context.streams[stream].arm_upstream_replay();
                // A keep-alive client transitions to Connected when it picks up
                // a new stream; it must not stay parked in KeepAlive.
                debug_assert!(
                    matches!(
                        self.position,
                        Position::Client(_, _, BackendStatus::Connected)
                    ),
                    "a reused keep-alive client must become Connected on start_stream"
                );
            }
            Position::Client(_, _, BackendStatus::Disconnecting) => {
                error!(
                    "{} start_stream called on Disconnecting H1 client",
                    log_context!(self)
                );
                return false;
            }
            Position::Client(_, _, _) => {}
            Position::Server => {
                error!(
                    "{} start_stream must not be called on H1 server connection",
                    log_context!(self)
                );
                return false;
            }
        }
        true
    }
}

#[cfg(test)]
mod tests {
    use std::{cell::Cell, cell::RefCell, io::Write, rc::Rc};

    use super::*;
    use crate::{
        Protocol as TransportKind,
        pool::Pool,
        protocol::{
            kawa_h1::editor::HttpContext,
            mux::{
                BackendId, BackendSlot, Connection,
                connection::{EndpointClient, EndpointServer},
                router::Router,
                tcp_info_reads,
                test_support::{connected_socket, test_context},
            },
        },
        socket::SessionTcpStream,
    };

    // ── The `peer=` slot of the MUX-H1 log prefix ───────────────────────
    //
    // These pin that the slot is the address the connection snapshotted at
    // construction, not a live `getpeername(2)` taken per expansion. The
    // difference is invisible while a connection is healthy and is the whole
    // story once it is not: `getpeername(2)` answers ENOTCONN after the peer
    // resets, and it reports the transport peer — the load balancer — on a
    // PROXY-protocol frontend.

    /// Address the handler caches. Deliberately non-loopback so it cannot
    /// collide with whatever ephemeral port a live lookup would report.
    const CACHED_PEER: &str = "10.0.0.42:12345";

    fn cached_peer() -> std::net::SocketAddr {
        CACHED_PEER
            .parse()
            .expect("the cached peer literal must parse")
    }

    /// A live, established loopback connection plus the address
    /// `getpeername(2)` reports for it. The listener is returned so the
    /// connection stays up for the whole test: these tests assert the snapshot
    /// wins even while the live lookup is perfectly healthy, so a half-dead
    /// socket would weaken them rather than strengthen them.
    fn connected_loopback_stream() -> (
        std::net::TcpListener,
        mio::net::TcpStream,
        std::net::SocketAddr,
    ) {
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
        (listener, mio::net::TcpStream::from_std(stream), live_peer)
    }

    /// A socket that was never connected, and therefore one whose
    /// `getpeername(2)` fails with `ENOTCONN` *deterministically*.
    ///
    /// This is the one reliable way to stage a failing live lookup. The
    /// obvious alternative — connect to a closed port and wait for the RST —
    /// is a race: whether the kernel has collected the RST yet, and so whether
    /// `getpeername(2)` has started refusing, is not something a test may
    /// assert on. An unconnected socket is in the refusing state from birth,
    /// which is the state a reset peer leaves behind and the state these
    /// tests are about.
    fn unconnected_stream() -> mio::net::TcpStream {
        let socket = socket2::Socket::new(
            socket2::Domain::IPV4,
            socket2::Type::STREAM,
            Some(socket2::Protocol::TCP),
        )
        .expect("a test socket must be creatable");
        socket
            .set_nonblocking(true)
            .expect("mio requires a nonblocking stream");
        mio::net::TcpStream::from_std(std::net::TcpStream::from(socket))
    }

    /// An opaque backend id for a `Position::Client` connection. The slot is
    /// never resolved by anything under test — `log_context!` renders
    /// `position` and nothing looks the backend up — so a standalone id is
    /// enough and spares these tests a whole `Mux` and backend registry.
    fn test_backend_id(address: std::net::SocketAddr) -> BackendId {
        BackendId {
            slot: BackendSlot(0),
            backend_id: Rc::from("test-backend"),
            address,
        }
    }

    /// Unwrap the H1 arm of a freshly built connection.
    fn h1_of<Front: SocketHandler>(connection: Connection<Front>) -> ConnectionH1<Front> {
        match connection {
            Connection::H1(connection) => connection,
            Connection::H2(_) => unreachable!("new_h1_* builds an H1 connection"),
        }
    }

    /// Symptom 1 — `getpeername(2)` answers `ENOTCONN` once the peer has
    /// reset, so the pre-fix macro rendered `peer=None` on exactly the
    /// `error!` lines an operator reads during an incident. The address was
    /// least available precisely when it mattered most.
    ///
    /// Staged with a never-connected socket rather than a reset one, because
    /// only the former refuses deterministically — see [`unconnected_stream`].
    /// A peer reset is one way to reach that state; what this pins is the
    /// rendering once the socket is in it, which is the part the macro owns.
    ///
    /// To SEE THIS RED: in `log_context!`, put
    /// `peer = $self.socket.socket_ref().peer_addr().ok(),` back in place of
    /// `peer = $self.peer_address,`. The slot collapses to `peer=None`.
    #[test]
    fn log_context_renders_the_peer_when_the_live_lookup_fails() {
        let stream = unconnected_stream();
        let cached = cached_peer();

        // Premise: the live lookup really is refusing, so the assertion below
        // cannot pass for the wrong reason.
        assert_eq!(
            stream.peer_addr().ok(),
            None,
            "an unconnected socket must refuse getpeername(2)"
        );

        let session_ulid = Ulid::generate();
        let connection = h1_of(Connection::new_h1_server(
            session_ulid,
            SessionTcpStream::new(stream, session_ulid, Some(cached)),
            Duration::from_secs(30),
        ));

        let rendered = log_context!(connection);

        assert!(
            rendered.contains(&format!("peer=Some({cached})")),
            "the MUX-H1 peer= slot must survive a failed live lookup: {rendered}"
        );
        assert!(
            !rendered.contains("peer=None"),
            "the MUX-H1 peer= slot must not collapse to None while a snapshot \
             exists: {rendered}"
        );
    }

    /// Symptom 2 — a backend connection is built from a nonblocking
    /// `connect()` that has not completed, so `getpeername(2)` refuses for the
    /// whole window in which an ECONNREFUSED line is emitted. The dial path
    /// caches the cluster-configured backend address into
    /// `SessionTcpStream::configured_peer` for exactly that reason, and the H1
    /// macro has to consult it rather than the raw stream.
    ///
    /// This is the `Position::Client` constructor, which `ConnectionH2` has no
    /// counterpart for: `ConnectionH1` serves backend connections too.
    ///
    /// To SEE THIS RED: in `log_context!`, put
    /// `peer = $self.socket.socket_ref().peer_addr().ok(),` back in place of
    /// `peer = $self.peer_address,`. The backend id the operator needs is
    /// replaced by `peer=None`.
    #[test]
    fn log_context_renders_the_backend_peer_while_the_connect_is_in_flight() {
        let stream = unconnected_stream();
        let backend_address = cached_peer();

        // Premise: this is the state a dial in flight really leaves the socket
        // in — the live lookup refuses.
        assert_eq!(
            stream.peer_addr().ok(),
            None,
            "a connect() still in flight must refuse getpeername(2)"
        );

        let session_ulid = Ulid::generate();
        let connection = h1_of(Connection::new_h1_client(
            session_ulid,
            SessionTcpStream::new(stream, session_ulid, Some(backend_address)),
            "test-cluster".into(),
            test_backend_id(backend_address),
            Duration::from_secs(30),
        ));

        let rendered = log_context!(connection);

        assert!(
            rendered.contains(&format!("peer=Some({backend_address})")),
            "a backend MUX-H1 line must name the configured backend: {rendered}"
        );
        assert!(
            !rendered.contains("peer=None"),
            "a backend MUX-H1 line must not collapse to None while the dial is \
             in flight: {rendered}"
        );
    }

    /// Symptom 3 — on a PROXY-protocol frontend the cached address is the
    /// advertised client while the socket's own peer is the load balancer.
    /// `upgrade_expect` adopts the advertised source into the handler's
    /// `configured_peer`, and the pre-fix macro read straight past it: for one
    /// request id the `HTTP` line named the client and the `MUX-H1` line named
    /// the load balancer.
    ///
    /// The live lookup is deliberately HEALTHY here and disagrees with the
    /// cache, which is what separates this from
    /// [`log_context_renders_the_peer_when_the_live_lookup_fails`] — a single
    /// test cannot be red for both reasons.
    ///
    /// This stages the cleartext expect-proxy route, whose handler really is a
    /// `SessionTcpStream`. The TLS route composes the same macro with
    /// `FrontRustls::peer_addr`, whose own PROXY seeding is pinned in
    /// `https.rs` by `front_rustls_peer_snapshot_is_the_session_peer_not_the_accepted_socket`.
    ///
    /// To SEE THIS RED: in `log_context!`, put
    /// `peer = $self.socket.socket_ref().peer_addr().ok(),` back in place of
    /// `peer = $self.peer_address,`. The rendered line then carries the
    /// loopback address the socket is really connected to — the load balancer
    /// — so the first assertion fails on the missing advertised client.
    #[test]
    fn log_context_renders_the_proxy_advertised_peer_not_the_load_balancer() {
        let (_listener, stream, live_peer) = connected_loopback_stream();
        let advertised = cached_peer();

        // Premise: the live lookup is healthy and disagrees with the cache, so
        // the assertions below cannot pass for the wrong reason.
        assert_eq!(
            stream.peer_addr().ok(),
            Some(live_peer),
            "the test socket must be genuinely connected, so a live lookup succeeds"
        );
        assert_ne!(
            advertised, live_peer,
            "the advertised and transport addresses must differ for this test to discriminate"
        );

        let session_ulid = Ulid::generate();
        let connection = h1_of(Connection::new_h1_server(
            session_ulid,
            SessionTcpStream::new(stream, session_ulid, Some(advertised)),
            Duration::from_secs(30),
        ));

        let rendered = log_context!(connection);

        assert!(
            rendered.contains(&format!("peer=Some({advertised})")),
            "MUX-H1 must render the PROXY-advertised client: {rendered}"
        );
        assert!(
            !rendered.contains(&live_peer.to_string()),
            "MUX-H1 must not fall back to the transport peer, which is the load \
             balancer on a PROXY frontend: {rendered}"
        );
    }

    /// The per-stream twin fills the same slot from the same snapshot.
    ///
    /// `log_context_stream!` has no production expansion in this file today
    /// and carries `#[allow(unused_macros)]`, so nothing else type-checks its
    /// body at all. That is the reason to pin it rather than a reason to skip
    /// it: leaving twin macros on different sources is how they drift apart,
    /// and the divergence would be worse than either being wrong alone — the
    /// same session would render two different peers depending on whether a
    /// stream happened to be in scope at the callsite.
    ///
    /// To SEE THIS RED: in `log_context_stream!`, put
    /// `peer = $self.socket.socket_ref().peer_addr().ok(),` back in place of
    /// `peer = $self.peer_address,`. The per-stream envelope then carries the
    /// loopback address while the connection envelope carries the cache.
    #[test]
    fn log_context_stream_renders_the_snapshot_not_a_live_lookup() {
        let (_listener, stream, live_peer) = connected_loopback_stream();
        let advertised = cached_peer();

        assert_eq!(
            stream.peer_addr().ok(),
            Some(live_peer),
            "the test socket must be genuinely connected, so a live lookup succeeds"
        );

        let session_ulid = Ulid::generate();
        let connection = h1_of(Connection::new_h1_server(
            session_ulid,
            SessionTcpStream::new(stream, session_ulid, Some(advertised)),
            Duration::from_secs(30),
        ));
        let http_context = test_http_context(session_ulid);

        let rendered = log_context_stream!(connection, http_context);

        assert!(
            rendered.contains(&format!("peer=Some({advertised})")),
            "the per-stream MUX-H1 envelope must render the snapshot: {rendered}"
        );
        assert!(
            !rendered.contains(&live_peer.to_string()),
            "the per-stream MUX-H1 envelope must not fall back to the live \
             getpeername(2) answer: {rendered}"
        );
    }

    /// A [`SocketHandler`] that counts how often it is asked for the peer
    /// address, delegating every other method to a real loopback stream.
    ///
    /// The tests above pin WHICH address the log prefix renders. None of them
    /// can see how many times the connection reaches into the socket to get
    /// it, because both production handlers answer from a cache and so give
    /// the same answer however often they are asked. This handler makes the
    /// count observable.
    struct PeerAddrCountingSocket {
        stream: mio::net::TcpStream,
        peer: std::net::SocketAddr,
        calls: Rc<Cell<usize>>,
    }

    impl SocketHandler for PeerAddrCountingSocket {
        fn socket_read(&mut self, buf: &mut [u8]) -> (usize, SocketResult) {
            self.stream.socket_read(buf)
        }

        fn socket_write(&mut self, buf: &[u8]) -> (usize, SocketResult) {
            self.stream.socket_write(buf)
        }

        fn socket_write_vectored(&mut self, bufs: &[IoSlice]) -> (usize, SocketResult) {
            self.stream.socket_write_vectored(bufs)
        }

        fn socket_ref(&self) -> &mio::net::TcpStream {
            &self.stream
        }

        fn socket_mut(&mut self) -> &mut mio::net::TcpStream {
            &mut self.stream
        }

        fn peer_addr(&self) -> Option<std::net::SocketAddr> {
            self.calls.set(self.calls.get() + 1);
            Some(self.peer)
        }

        fn protocol(&self) -> crate::socket::TransportProtocol {
            crate::socket::TransportProtocol::Tcp
        }

        fn read_error(&self) {}

        fn write_error(&self) {}
    }

    /// The `peer=` slot is read from the socket exactly ONCE per connection —
    /// at construction — however many log lines the connection renders.
    ///
    /// This is the property that makes `peer_address` worth having, and it is
    /// the only one that separates the snapshot from simply calling the trait
    /// method in the macro. It is not about which address appears (the tests
    /// above own that): for a handler that already caches, reading the cache
    /// and reading a snapshot taken from that cache agree by construction, so
    /// substituting `peer = $self.socket.peer_addr(),` leaves every one of
    /// them green. What the snapshot changed is WHICH object a log line reads,
    /// and only a call count can observe that.
    ///
    /// TO SEE THIS RED: in `log_context!`, put
    /// `peer = $self.socket.peer_addr(),` — the call-the-trait-method shape —
    /// in place of `peer = $self.peer_address,`. The count then rises by one
    /// per rendered line and the final assertion fails with
    /// `the peer address must be read once, at construction, not once per log line:
    /// left: 4, right: 1`.
    #[test]
    fn log_context_reads_the_peer_address_once_per_connection() {
        let (_listener, stream, _live_peer) = connected_loopback_stream();
        let calls = Rc::new(Cell::new(0usize));
        let peer = cached_peer();
        let socket = PeerAddrCountingSocket {
            stream,
            peer,
            calls: Rc::clone(&calls),
        };

        let connection = h1_of(Connection::new_h1_server(
            Ulid::generate(),
            socket,
            Duration::from_secs(30),
        ));

        assert_eq!(
            calls.get(),
            1,
            "construction takes exactly one peer_addr() snapshot"
        );

        // Premise: the rendered lines really do carry the address, so a zero
        // count below would mean the slot went missing rather than that it
        // became free.
        for _ in 0..3 {
            let rendered = log_context!(connection);
            assert!(
                rendered.contains(&format!("peer=Some({peer})")),
                "each rendered line must still carry the peer: {rendered}"
            );
        }

        assert_eq!(
            calls.get(),
            1,
            "the peer address must be read once, at construction, not once per log line"
        );
    }

    /// The three fields `log_context_stream!` reads out of an `HttpContext`,
    /// with every other field at an inert default. Built directly rather than
    /// through a `Stream`, because the macro touches no buffer and a real
    /// stream would drag a whole `Pool` in behind it.
    fn test_http_context(session_ulid: Ulid) -> HttpContext {
        HttpContext {
            keep_alive_backend: true,
            keep_alive_frontend: true,
            sticky_session_found: None,
            method: None,
            authority: None,
            path: None,
            status: None,
            reason: None,
            user_agent: None,
            x_request_id: None,
            xff_chain: None,
            #[cfg(feature = "opentelemetry")]
            otel: None,
            closing: false,
            session_id: session_ulid,
            id: Ulid::generate(),
            backend_id: None,
            cluster_id: None,
            affinity_key: None,
            protocol: TransportKind::HTTP,
            public_address: "127.0.0.1:0"
                .parse()
                .expect("the public address literal must parse"),
            session_address: None,
            sticky_name: String::new(),
            sticky_session: None,
            backend_address: None,
            tls_server_name: None,
            tls_cert_names: None,
            strict_sni_binding: false,
            elide_x_real_ip: false,
            send_x_real_ip: false,
            forwarded_headers: sozu_command_lib::proto::command::ForwardedHeaders::Both,
            max_trailer_fields: 128,
            trailer_fields: 0,
            tls_version: None,
            tls_cipher: None,
            tls_alpn: None,
            sozu_id_header: String::from("Sozu-Id"),
            redirect_location: None,
            www_authenticate: None,
            original_authority: None,
            headers_response: Vec::new(),
            retry_after_seconds: None,
            frontend_redirect_template: None,
            redirect_status: None,
            tags: None,
            forwarding_hop: None,
            access_log_message: None,
            backends_unavailable: false,
        }
    }

    /// `ConnectionH1`'s `Debug` renders the peer address the connection
    /// snapshotted at construction, not the transport address the kernel knows.
    ///
    /// This is the twin of `ConnectionH2`'s `Debug`, which already renders
    /// `peer_address` and names no socket at all. `ConnectionH1` was the last
    /// `Debug` in this module handing `mio::net::TcpStream`'s own `Debug` a
    /// `socket` field through `SocketHandler::socket_ref` — a local address, a
    /// peer address and a file descriptor — and so the last one whose rendered
    /// line could disagree with the `peer=` slot every `log_context!` line of
    /// the same connection already carries.
    ///
    /// Staged like
    /// [`log_context_renders_the_proxy_advertised_peer_not_the_load_balancer`]:
    /// a genuinely connected loopback socket whose live lookup is healthy and
    /// disagrees with the address its handler declares, which is the
    /// PROXY-protocol frontend's shape. This pins the VALUE rendered rather
    /// than a count — the two addresses differ, so only one of them can appear.
    ///
    /// To SEE THIS RED: in `impl Debug for ConnectionH1`, put
    /// `.field("socket", &self.socket.socket_ref())` back in place of
    /// `.field("peer_address", &self.peer_address)`. The rendered struct then
    /// carries the loopback address the socket is really connected to — the
    /// load balancer — so the first assertion fails on the missing advertised
    /// client and the second on the transport address being present.
    #[test]
    fn debug_renders_the_proxy_advertised_peer_not_the_transport_socket() {
        let (_listener, stream, live_peer) = connected_loopback_stream();
        let advertised = cached_peer();

        // Premise: the live lookup is healthy and disagrees with the declared
        // address, so the assertions below cannot pass for the wrong reason.
        assert_eq!(
            stream.peer_addr().ok(),
            Some(live_peer),
            "the test socket must be genuinely connected, so a live lookup succeeds"
        );
        assert_ne!(
            advertised, live_peer,
            "the declared and transport addresses must differ for this test to discriminate"
        );

        let session_ulid = Ulid::generate();
        let connection = h1_of(Connection::new_h1_server(
            session_ulid,
            SessionTcpStream::new(stream, session_ulid, Some(advertised)),
            Duration::from_secs(30),
        ));

        let rendered = format!("{connection:?}");

        assert!(
            rendered.contains(&format!("Some({advertised})")),
            "the ConnectionH1 Debug must render the declared peer: {rendered}"
        );
        assert!(
            !rendered.contains(&live_peer.to_string()),
            "the ConnectionH1 Debug must not render the transport address, which \
             is the load balancer on a PROXY frontend: {rendered}"
        );
    }

    // ── `backend_header_time`, the H1 half (sozu-proxy/sozu#426) ─────────
    //
    // DO NOT READ THESE TESTS AS COVERING H2. They drive
    // `ConnectionH1::readable` only. The H2 twin lives in
    // `ConnectionH2::handle_headers_frame` (`lib/src/protocol/mux/h2.rs`) and
    // is pinned by its own test there.

    /// A `Position::Client` H1 backend connection whose stream is linked to a
    /// frontend, staged at the exact moment a real dial completes: the
    /// connection is `Connected`, `backend_connected` is armed, and not one
    /// response byte has arrived.
    ///
    /// Both loopback peers are returned and must be held for the whole test —
    /// dropping either tears the connection down and turns the next
    /// `readable()` into a forced disconnect instead of a parse.
    /// Held together rather than returned as a tuple, the way
    /// `LedgerFixture` (`lib/src/protocol/mux/h2.rs`) is: the buffer pool and
    /// both loopback peers are lifetime ballast with no business in a test
    /// body, and a six-element tuple is `clippy::type_complexity`.
    struct BackendReadFixture {
        context: Context<crate::protocol::mux::test_support::TestListener>,
        frontend: Connection<mio::net::TcpStream>,
        client: ConnectionH1<mio::net::TcpStream>,
        backend_peer: std::net::TcpStream,
        _frontend_peer: std::net::TcpStream,
        _pool: Rc<RefCell<Pool>>,
    }

    fn linked_backend_connection() -> BackendReadFixture {
        let pool = Rc::new(RefCell::new(Pool::with_capacity(4, 8, 16384)));
        let mut context = test_context(&pool);
        context
            .create_stream(Ulid::generate(), 1 << 16)
            .expect("the test pool must hand out stream buffers");

        let (front_socket, front_peer) = connected_socket();
        let frontend =
            Connection::new_h1_server(Ulid::generate(), front_socket, Duration::from_secs(60));

        let (back_socket, back_peer) = connected_socket();
        let session_ulid = Ulid::generate();
        let mut client = h1_of(Connection::new_h1_client(
            session_ulid,
            back_socket,
            "test-cluster".into(),
            test_backend_id(cached_peer()),
            Duration::from_secs(60),
        ));
        // `new_h1_client` opens in `Connecting`; move it the way a completed
        // dial does, so the read path runs its `Connected` accounting.
        if let Position::Client(_, _, status) = &mut client.position {
            *status = BackendStatus::Connected;
        }
        client.stream = Some(0);

        let stream = &mut context.streams[0];
        stream.state = StreamState::Linked(mio::Token(1));
        stream.metrics.backend_id = Some("test-backend".into());
        stream.metrics.backend_start();
        stream.metrics.backend_connected();

        BackendReadFixture {
            context,
            frontend,
            client,
            backend_peer: back_peer,
            _frontend_peer: front_peer,
            _pool: pool,
        }
    }

    /// Hand `bytes` to the backend peer and drive `ConnectionH1::readable`
    /// until `done` holds, bounded.
    ///
    /// The loop is not a retry papering over a flaky assertion: a loopback
    /// write is a syscall away, not instantaneous, so a single `readable()`
    /// may legitimately see `WouldBlock` and read nothing. The bound is what
    /// keeps a genuine failure a failure instead of a hang.
    fn feed_backend<F>(
        peer: &mut std::net::TcpStream,
        client: &mut ConnectionH1<mio::net::TcpStream>,
        context: &mut Context<crate::protocol::mux::test_support::TestListener>,
        frontend: &mut Connection<mio::net::TcpStream>,
        bytes: &[u8],
        done: F,
    ) where
        F: Fn(&Context<crate::protocol::mux::test_support::TestListener>) -> bool,
    {
        peer.write_all(bytes)
            .expect("the loopback peer must accept the staged response bytes");
        for _ in 0..64 {
            client.readiness.event.insert(Ready::READABLE);
            client.readable(context, EndpointServer(frontend));
            if done(context) {
                return;
            }
        }
        panic!("the backend read path never reached the staged state");
    }

    /// The whole point of the metric, pinned as an ordering: the H1 backend
    /// time-to-first-header-byte instant is taken when the response HEADERS
    /// finish parsing, which is strictly before the response ENDS.
    ///
    /// Two phases, and the split is what makes this discriminating. After the
    /// headers alone (`Content-Length: 2`, body withheld) the header instant
    /// must already exist while `backend_stop` — the response-end marker every
    /// one of `ConnectionH1::writable`'s five sites owns — must still be
    /// absent. An instant placed at any of those sites cannot satisfy that.
    /// After the body, the instant must be UNCHANGED: the transition fires on
    /// the header -> body edge, not on every read.
    ///
    /// The ordering assertion is then `header_time <= response_time` on
    /// `Duration`s, not on the millisecond values the histogram stores: at
    /// millisecond resolution a fast test rounds both to 0 and the relation
    /// would hold however wrong the placement is.
    ///
    /// `backend_stop` is marked by the test rather than driven through
    /// `ConnectionH1::writable` because that pass calls
    /// `SessionMetrics::reset` immediately after its access log, which wipes
    /// every instant before a test could read one.
    ///
    /// To SEE THIS RED: move `parts.metrics.backend_headers_received();` out
    /// of the `!was_main_phase && self.position.is_client()` branch of
    /// `ConnectionH1::readable` and put it immediately after the
    /// `stream.metrics.backend_stop();` that precedes `Some("H1::Complete")`
    /// in `ConnectionH1::writable`. Phase one then reports no header time at
    /// all.
    #[test]
    fn the_h1_backend_header_instant_lands_on_the_headers_not_the_response_end() {
        let mut fixture = linked_backend_connection();
        let BackendReadFixture {
            context,
            frontend,
            client,
            backend_peer,
            ..
        } = &mut fixture;

        // Premise: nothing has been received, so neither instant can be stale.
        assert_eq!(context.streams[0].metrics.backend_header_time(), None);
        assert_eq!(context.streams[0].metrics.backend_stop, None);

        feed_backend(
            backend_peer,
            client,
            context,
            frontend,
            b"HTTP/1.1 200 OK\r\nContent-Length: 2\r\n\r\n",
            |context| context.streams[0].back.is_main_phase(),
        );

        let after_headers = context.streams[0]
            .metrics
            .backend_headers_received
            .expect("the response headers must arm the backend header instant");
        assert!(
            !context.streams[0].back.is_terminated(),
            "premise: the response is not over — its 2-byte body is still \
             outstanding, so this phase cannot be observing the response end"
        );
        assert_eq!(
            context.streams[0].metrics.backend_stop, None,
            "the response-end marker must still be absent while only the \
             headers have arrived"
        );

        feed_backend(backend_peer, client, context, frontend, b"ok", |context| {
            context.streams[0].back.is_terminated()
        });

        assert_eq!(
            context.streams[0].metrics.backend_headers_received,
            Some(after_headers),
            "the instant is taken on the header -> body edge, so the body \
             reads must not move it"
        );

        // What the `Some("H1::Complete")` arm of `ConnectionH1::writable` does
        // once the whole response has been forwarded.
        context.streams[0].metrics.backend_stop();

        let metrics = &context.streams[0].metrics;
        let header_time = metrics
            .backend_header_time()
            .expect("a completed H1 response must report a backend header time");
        let response_time = metrics
            .backend_response_time()
            .expect("a completed H1 response must report a backend response time");
        assert!(
            header_time <= response_time,
            "the first response-header byte arrives before the last response \
             byte: header_time {header_time:?} must not exceed response_time \
             {response_time:?}"
        );
    }

    // ── The access log of a request the parse rejected (sozu-proxy/sozu#1085) ──
    //
    // DO NOT READ THESE TESTS AS COVERING H2. They drive a `Position::Server`
    // `ConnectionH1` only. An H2 request whose field block `handle_header`
    // (`lib/src/protocol/mux/pkawa.rs`) refuses takes another path,
    // `record_rejected_request` in the same file, and is covered by the
    // `h2` tests beside `rejected_request_line`
    // (`lib/src/protocol/mux/stream.rs`).

    /// Drive `request` through a real `Position::Server` `ConnectionH1`, the
    /// way a client socket does: `readable` until the parse is rejected, then
    /// `writable` until the 400 is written and its access log emitted.
    /// Returns every log line the run produced.
    ///
    /// Driving both halves, rather than calling `Stream::generate_access_log`
    /// on a staged stream, is what makes these tests say which site emits the
    /// line and that the request bytes are still in the front buffer when it
    /// does.
    ///
    /// The loops are bounded for the reason `feed_backend` gives: a loopback
    /// write is a syscall away, not instantaneous.
    fn access_log_of_a_rejected_h1_request(request: &'static [u8]) -> String {
        crate::capture_test_logs(move || {
            let pool = Rc::new(RefCell::new(Pool::with_capacity(4, 8, 16384)));
            let mut context = test_context(&pool);
            context
                .create_stream(Ulid::generate(), 1 << 16)
                .expect("the test pool must hand out stream buffers");
            let (front_socket, mut front_peer) = connected_socket();
            let mut server = h1_of(Connection::new_h1_server(
                Ulid::generate(),
                front_socket,
                Duration::from_secs(60),
            ));
            server.stream = Some(0);
            let mut router = Router::new(Duration::from_secs(10), Duration::from_secs(10));

            front_peer
                .write_all(request)
                .expect("the loopback peer must accept the staged request bytes");
            for _ in 0..64 {
                server.readiness.event.insert(Ready::READABLE);
                server.readable(&mut context, EndpointClient(&mut router));
                if context.streams[0].front.is_error() {
                    break;
                }
            }
            assert!(
                context.streams[0].front.is_error(),
                "premise: the staged request must be rejected by the parse"
            );
            assert_eq!(
                context.streams[0].context.method, None,
                "premise: the rejection happened before `HttpContext` captured \
                 the request line, which is the whole defect"
            );

            // `ConnectionH1::writable` resets the stream metrics right after
            // it emits the access log, so a cleared start instant is the
            // observable "the line went out".
            for _ in 0..64 {
                server.readiness.event.insert(Ready::WRITABLE);
                server.writable(&mut context, EndpointClient(&mut router));
                if context.streams[0].metrics.start.is_none() {
                    break;
                }
            }
        })
    }

    /// A request that fails to parse after its response started on the wire
    /// (a backend answering early and streaming its body while the client
    /// still uploads a chunked body, then a malformed chunk) must cut that
    /// response, never splice a complete `HTTP/1.1 400` into its body, which
    /// the client would read as part of the first response.
    ///
    /// TO SEE THIS RED: in `ConnectionH1::readable`, delete the
    /// `Position::Server if stream.back.consumed` arm. The 400 is then
    /// written: `no 400 may follow a response already on the wire`.
    #[test]
    fn a_request_error_after_the_response_started_cuts_it_instead_of_splicing_a_400() {
        use std::io::Read;

        let pool = Rc::new(RefCell::new(Pool::with_capacity(4, 8, 16384)));
        let mut context = test_context(&pool);
        context
            .create_stream(Ulid::generate(), 1 << 16)
            .expect("the test pool must hand out stream buffers");
        let (front_socket, mut front_peer) = connected_socket();
        front_peer
            .set_nonblocking(true)
            .expect("the loopback peer must switch to non-blocking");
        let mut server = h1_of(Connection::new_h1_server(
            Ulid::generate(),
            front_socket,
            Duration::from_secs(60),
        ));
        server.stream = Some(0);
        let mut router = Router::new(Duration::from_secs(10), Duration::from_secs(10));
        // The early response already put bytes on the client's wire.
        context.streams[0].back.consumed = true;

        front_peer
            .write_all(
                b"POST /upload HTTP/1.1\r\nHost: localhost\r\nTransfer-Encoding: chunked\r\n\r\nZZ\r\n",
            )
            .expect("the loopback peer must accept the staged request bytes");
        let mut result = MuxResult::Continue;
        for _ in 0..64 {
            server.readiness.event.insert(Ready::READABLE);
            result = server.readable(&mut context, EndpointClient(&mut router));
            if context.streams[0].front.is_error() {
                break;
            }
        }
        assert!(
            context.streams[0].front.is_error(),
            "premise: the malformed chunk must be rejected by the parse"
        );

        let mut received = Vec::new();
        for _ in 0..16 {
            server.readiness.event.insert(Ready::WRITABLE);
            if server.readiness.filter_interest().is_writable() {
                server.writable(&mut context, EndpointClient(&mut router));
            }
            let mut buf = [0u8; 4096];
            if let Ok(n) = front_peer.read(&mut buf) {
                received.extend_from_slice(&buf[..n]);
            }
        }
        assert!(
            !received.windows(12).any(|w| w == b"HTTP/1.1 400"),
            "no 400 may follow a response already on the wire, got {:?}",
            String::from_utf8_lossy(&received)
        );
        assert_ne!(
            context.streams[0].context.status,
            Some(400),
            "the started response must not be replaced by a 400"
        );
        assert!(
            context.streams[0].back.is_error() && matches!(result, MuxResult::CloseSession),
            "the response is cut and the session closes, got {:?} / {:?}",
            context.streams[0].back.parsing_phase,
            result
        );
    }

    /// A request that fails to parse after its stream was linked, and after
    /// the backend's complete response already started on the wire, cuts that
    /// response and also ends the backend stream (sozu-proxy/sozu#1716). The
    /// backend connection holds part of the rejected request, so it must leave
    /// the `backend_streams` reverse index and be disconnected. It must not stay
    /// attached to the stream slot, and must not go back to the keep-alive pool,
    /// although its response is complete.
    ///
    /// TO SEE THIS RED: in `ConnectionH1::readable`, delete the block of the
    /// `Position::Server if stream.back.consumed` arm that clears
    /// `keep_alive_backend` and calls `endpoint.end_stream`. The reverse index
    /// then still lists the stream: `the rejected stream must leave the
    /// backend reverse index`. Keeping `end_stream` but not the flag instead
    /// pools the connection: `a backend holding part of a rejected request
    /// must be disconnected, not pooled`.
    #[test]
    fn a_request_error_after_a_linked_response_started_ends_the_backend_stream() {
        const BACKEND: mio::Token = mio::Token(1);

        let pool = Rc::new(RefCell::new(Pool::with_capacity(4, 8, 16384)));
        let mut context = test_context(&pool);
        context
            .create_stream(Ulid::generate(), 1 << 16)
            .expect("the test pool must hand out stream buffers");
        let (front_socket, mut front_peer) = connected_socket();
        let mut frontend =
            Connection::new_h1_server(Ulid::generate(), front_socket, Duration::from_secs(60));
        if let Connection::H1(server) = &mut frontend {
            server.stream = Some(0);
        }
        let (back_socket, mut back_peer) = connected_socket();
        let backend_ulid = Ulid::generate();
        let mut client = h1_of(Connection::new_h1_client(
            backend_ulid,
            SessionTcpStream::new(back_socket, backend_ulid, None),
            "test-cluster".into(),
            test_backend_id(cached_peer()),
            Duration::from_secs(60),
        ));
        if let Position::Client(_, _, status) = &mut client.position {
            *status = BackendStatus::KeepAlive;
        }
        let mut router = Router::new(Duration::from_secs(10), Duration::from_secs(10));

        // The request head and a first chunk: the stream is routed.
        front_peer
            .write_all(
                b"POST /upload HTTP/1.1\r\nHost: localhost\r\nTransfer-Encoding: chunked\r\n\r\n5\r\nHello\r\n",
            )
            .expect("the loopback peer must accept the staged request head");
        for _ in 0..64 {
            frontend.readiness_mut().event.insert(Ready::READABLE);
            frontend.readable(&mut context, EndpointClient(&mut router));
            if context.streams[0].state == StreamState::Link {
                break;
            }
        }
        assert_eq!(
            context.streams[0].state,
            StreamState::Link,
            "premise: the request head must be parsed and queued for linking"
        );

        // What the router does with a pooled backend connection.
        assert!(
            client.start_stream(0, &mut context),
            "premise: the pooled backend must accept the stream"
        );
        context.link_stream(0, BACKEND);
        router.backends.insert(BACKEND, Connection::H1(client));

        // The backend answers early and completely, and that response has
        // started on the client's wire.
        back_peer
            .write_all(b"HTTP/1.1 200 OK\r\nContent-Length: 2\r\n\r\nok")
            .expect("the loopback backend peer must accept the response");
        for _ in 0..64 {
            let Some(backend) = router.backends.get_mut(&BACKEND) else {
                unreachable!("the backend was inserted")
            };
            backend.readiness_mut().event.insert(Ready::READABLE);
            backend.readable(&mut context, EndpointServer(&mut frontend));
            if context.streams[0].back.is_terminated() {
                break;
            }
        }
        assert!(
            context.streams[0].back.is_terminated(),
            "premise: the backend response must be read whole"
        );
        context.streams[0].back.consumed = true;

        // A malformed chunk-size line rejects the request.
        front_peer
            .write_all(b"ZZ\r\n")
            .expect("the loopback peer must accept the malformed chunk");
        let mut result = MuxResult::Continue;
        for _ in 0..64 {
            frontend.readiness_mut().event.insert(Ready::READABLE);
            result = frontend.readable(&mut context, EndpointClient(&mut router));
            if context.streams[0].front.is_error() {
                break;
            }
        }
        assert!(
            context.streams[0].front.is_error(),
            "premise: the malformed chunk must be rejected by the parse"
        );
        assert!(
            matches!(result, MuxResult::CloseSession),
            "the started response is cut and the session closes, got {result:?}"
        );

        assert!(
            context
                .backend_streams
                .get(&BACKEND)
                .is_none_or(|ids| !ids.contains(&0)),
            "the rejected stream must leave the backend reverse index"
        );
        let Some(Connection::H1(client)) = router.backends.get(&BACKEND) else {
            unreachable!("the backend was inserted as H1")
        };
        assert_eq!(
            client.stream, None,
            "the backend connection must no longer carry the rejected stream"
        );
        assert!(
            matches!(
                client.position,
                Position::Client(_, _, BackendStatus::Disconnecting)
            ),
            "a backend holding part of a rejected request must be disconnected, not pooled"
        );
    }

    /// A request that fails to parse after its stream was linked, before any
    /// byte of the response reached the client, is answered 400 and also ends
    /// the backend stream (sozu-proxy/sozu#1716). The backend connection holds
    /// part of the rejected request, so it must leave the `backend_streams`
    /// reverse index and be disconnected, not stay attached to the stream slot
    /// nor go back to the keep-alive pool, although its response is complete.
    ///
    /// TO SEE THIS RED: in `ConnectionH1::readable`, delete the block of the
    /// `Position::Server` arm (the one that answers 400) that clears the
    /// response and calls `endpoint.end_stream`. The reverse index then still
    /// lists the stream: `the rejected stream must leave the backend reverse
    /// index`.
    #[test]
    fn a_request_error_answered_400_after_linking_ends_the_backend_stream() {
        const BACKEND: mio::Token = mio::Token(1);

        let pool = Rc::new(RefCell::new(Pool::with_capacity(4, 8, 16384)));
        let mut context = test_context(&pool);
        context
            .create_stream(Ulid::generate(), 1 << 16)
            .expect("the test pool must hand out stream buffers");
        let (front_socket, mut front_peer) = connected_socket();
        let mut frontend =
            Connection::new_h1_server(Ulid::generate(), front_socket, Duration::from_secs(60));
        if let Connection::H1(server) = &mut frontend {
            server.stream = Some(0);
        }
        let (back_socket, mut back_peer) = connected_socket();
        let backend_ulid = Ulid::generate();
        let mut client = h1_of(Connection::new_h1_client(
            backend_ulid,
            SessionTcpStream::new(back_socket, backend_ulid, None),
            "test-cluster".into(),
            test_backend_id(cached_peer()),
            Duration::from_secs(60),
        ));
        if let Position::Client(_, _, status) = &mut client.position {
            *status = BackendStatus::KeepAlive;
        }
        let mut router = Router::new(Duration::from_secs(10), Duration::from_secs(10));

        // The request head and a first chunk: the stream is routed.
        front_peer
            .write_all(
                b"POST /upload HTTP/1.1\r\nHost: localhost\r\nTransfer-Encoding: chunked\r\n\r\n5\r\nHello\r\n",
            )
            .expect("the loopback peer must accept the staged request head");
        for _ in 0..64 {
            frontend.readiness_mut().event.insert(Ready::READABLE);
            frontend.readable(&mut context, EndpointClient(&mut router));
            if context.streams[0].state == StreamState::Link {
                break;
            }
        }
        assert_eq!(
            context.streams[0].state,
            StreamState::Link,
            "premise: the request head must be parsed and queued for linking"
        );

        // What the router does with a pooled backend connection.
        assert!(
            client.start_stream(0, &mut context),
            "premise: the pooled backend must accept the stream"
        );
        context.link_stream(0, BACKEND);
        router.backends.insert(BACKEND, Connection::H1(client));

        // The backend answers early and completely, but no byte of that
        // response has reached the client yet.
        back_peer
            .write_all(b"HTTP/1.1 200 OK\r\nContent-Length: 2\r\n\r\nok")
            .expect("the loopback backend peer must accept the response");
        for _ in 0..64 {
            let Some(backend) = router.backends.get_mut(&BACKEND) else {
                unreachable!("the backend was inserted")
            };
            backend.readiness_mut().event.insert(Ready::READABLE);
            backend.readable(&mut context, EndpointServer(&mut frontend));
            if context.streams[0].back.is_terminated() {
                break;
            }
        }
        assert!(
            context.streams[0].back.is_terminated(),
            "premise: the backend response must be read whole"
        );
        assert!(
            !context.streams[0].back.consumed,
            "premise: the response must not have started on the client's wire"
        );

        // A malformed chunk-size line rejects the request.
        front_peer
            .write_all(b"ZZ\r\n")
            .expect("the loopback peer must accept the malformed chunk");
        let mut result = MuxResult::Continue;
        for _ in 0..64 {
            frontend.readiness_mut().event.insert(Ready::READABLE);
            result = frontend.readable(&mut context, EndpointClient(&mut router));
            if context.streams[0].front.is_error() {
                break;
            }
        }
        assert!(
            context.streams[0].front.is_error(),
            "premise: the malformed chunk must be rejected by the parse"
        );
        assert!(
            matches!(result, MuxResult::Continue),
            "the request is answered 400 on the client connection, got {result:?}"
        );

        assert!(
            context
                .backend_streams
                .get(&BACKEND)
                .is_none_or(|ids| !ids.contains(&0)),
            "the rejected stream must leave the backend reverse index"
        );
        let Some(Connection::H1(client)) = router.backends.get(&BACKEND) else {
            unreachable!("the backend was inserted as H1")
        };
        assert_eq!(
            client.stream, None,
            "the backend connection must no longer carry the rejected stream"
        );
        assert!(
            matches!(
                client.position,
                Position::Client(_, _, BackendStatus::Disconnecting)
            ),
            "a backend holding part of a rejected request must be disconnected, not pooled"
        );
    }

    /// The access log of an H1 request carries the frontend round-trip time,
    /// the connection's one sample, taken here because this is its first
    /// line, and no backend one when the request never reached a backend.
    ///
    /// TO SEE THIS RED: in `ConnectionH1::writable`, at the `H1::Complete`
    /// access log that writes the rejected request's 400, replace the
    /// `self.rtt()` of `client_rtt` with `None`. The
    /// line then carries `-` in that cell and the first assertion fails.
    #[test]
    fn the_h1_access_log_carries_the_frontend_rtt() {
        let output =
            access_log_of_a_rejected_h1_request(b"GET /rtt HTTP/1.1\r\nBad Header: x\r\n\r\n");
        let line = output
            .lines()
            .find(|line| line.contains("GET /rtt 400"))
            .unwrap_or_else(|| panic!("the 400 must be logged, got: {output}"));
        let cells: Vec<&str> = line
            .split_whitespace()
            .find_map(|token| {
                let cells: Vec<&str> = token.split('/').collect();
                (cells.len() == 5).then_some(cells)
            })
            .unwrap_or_else(|| panic!("the line must carry the five durations: {line}"));
        assert_ne!(
            cells[3], "-",
            "the H1 access log must carry the frontend client_rtt, got: {line}"
        );
        assert_eq!(
            cells[4], "-",
            "a request that never reached a backend has no server_rtt, got: {line}"
        );
    }

    /// Twenty keep-alive requests on one frontend connection, each answered
    /// through the same backend connection taken back out of the keep-alive
    /// pool, cost one `getsockopt(TCP_INFO)` per side — not one per request
    /// and side — and every one of the twenty access lines still carries both
    /// RTTs, the same value on each line.
    ///
    /// Driven through the real `H1::Complete` site of
    /// `ConnectionH1::writable` and the real `EndpointClient::peer_rtt`, with
    /// the backend recycled by `ConnectionH1::end_stream` and
    /// `ConnectionH1::start_stream` exactly as a `Mux` does between two
    /// requests. The request side is staged rather than parsed: what is under
    /// test is the log emission, not routing.
    ///
    /// TO SEE THIS RED: in `memoized_rtt` (`lib/src/protocol/mux/mod.rs`),
    /// delete the early return on `memo.get()`. Every request then reads both
    /// sockets again and the count assertion fails with `left: 40`,
    /// `right: 2`.
    #[test]
    fn keep_alive_requests_read_tcp_info_once_per_connection() {
        const REQUESTS: usize = 20;
        const BACKEND: mio::Token = mio::Token(1);
        let (reads_sender, reads_receiver) = std::sync::mpsc::channel();

        let output = crate::capture_test_logs(move || {
            let pool = Rc::new(RefCell::new(Pool::with_capacity(4, 8, 16384)));
            let mut context = test_context(&pool);
            context
                .create_stream(Ulid::generate(), 1 << 16)
                .expect("the test pool must hand out stream buffers");
            let (front_socket, mut front_peer) = connected_socket();
            front_peer
                .set_nonblocking(true)
                .expect("the frontend peer is drained without blocking");
            let mut frontend =
                Connection::new_h1_server(Ulid::generate(), front_socket, Duration::from_secs(60));
            let (back_socket, mut back_peer) = connected_socket();
            let backend_ulid = Ulid::generate();
            let mut client = h1_of(Connection::new_h1_client(
                backend_ulid,
                SessionTcpStream::new(back_socket, backend_ulid, None),
                "test-cluster".into(),
                test_backend_id(cached_peer()),
                Duration::from_secs(60),
            ));
            // The first request finds the connection freshly dialled; every
            // later one takes it back out of the pool through `start_stream`.
            if let Position::Client(_, _, status) = &mut client.position {
                *status = BackendStatus::KeepAlive;
            }
            let mut router = Router::new(Duration::from_secs(10), Duration::from_secs(10));
            router.backends.insert(BACKEND, Connection::H1(client));

            let before = tcp_info_reads();
            let mut drained = [0u8; 4096];
            for request in 0..REQUESTS {
                let Some(Connection::H1(client)) = router.backends.get_mut(&BACKEND) else {
                    unreachable!("the backend was inserted as H1")
                };
                assert!(
                    client.start_stream(0, &mut context),
                    "premise: the pooled backend must accept request {request}"
                );
                let stream = &mut context.streams[0];
                stream.state = StreamState::Linked(BACKEND);
                stream.context.keep_alive_frontend = true;
                // The staged request is complete and fully written: only then
                // may either connection outlive it (sozu-proxy/sozu#1721).
                stream.front.parsing_phase = kawa::ParsingPhase::Terminated;
                stream.metrics.service_start();
                stream.metrics.backend_id = Some("test-backend".into());
                stream.metrics.backend_start();
                stream.metrics.backend_connected();

                back_peer
                    .write_all(b"HTTP/1.1 200 OK\r\nContent-Length: 2\r\n\r\nok")
                    .expect("the loopback backend peer must accept the response");
                for _ in 0..64 {
                    client.readiness.event.insert(Ready::READABLE);
                    client.readable(&mut context, EndpointServer(&mut frontend));
                    if context.streams[0].back.is_terminated() {
                        break;
                    }
                }
                assert!(
                    context.streams[0].back.is_terminated(),
                    "premise: response {request} must be read whole"
                );

                for _ in 0..64 {
                    frontend.readiness_mut().event.insert(Ready::WRITABLE);
                    frontend.writable(&mut context, EndpointClient(&mut router));
                    if context.streams[0].metrics.start.is_none() {
                        break;
                    }
                }
                assert_eq!(
                    context.streams[0].state,
                    StreamState::Idle,
                    "premise: request {request} must complete through the \
                     keep-alive branch of `H1::Complete`"
                );
                while let Ok(read) = std::io::Read::read(&mut front_peer, &mut drained) {
                    if read == 0 {
                        break;
                    }
                }
            }
            reads_sender
                .send(tcp_info_reads() - before)
                .expect("the test thread must report its TCP_INFO reads");
        });

        let lines: Vec<&str> = output
            .lines()
            .filter(|line| line.contains("H1::Complete"))
            .collect();
        assert_eq!(
            lines.len(),
            REQUESTS,
            "every request must be logged, got: {output}"
        );
        let mut rtts = Vec::with_capacity(REQUESTS);
        for line in &lines {
            let cells: Vec<&str> = line
                .split_whitespace()
                .find_map(|token| {
                    let cells: Vec<&str> = token.split('/').collect();
                    (cells.len() == 5).then_some(cells)
                })
                .unwrap_or_else(|| panic!("the line must carry the five durations: {line}"));
            assert_ne!(cells[3], "-", "every line must carry client_rtt: {line}");
            assert_ne!(cells[4], "-", "every line must carry server_rtt: {line}");
            rtts.push((cells[3], cells[4]));
        }
        assert_eq!(
            reads_receiver
                .recv()
                .expect("the test thread must report its TCP_INFO reads"),
            2,
            "{REQUESTS} keep-alive requests must read TCP_INFO once for the \
             frontend and once for the backend"
        );
        assert!(
            rtts.windows(2).all(|pair| pair[0] == pair[1]),
            "every request of one connection reports that connection's one \
             sample, got: {rtts:?}"
        );
    }

    /// A backend connection that answered before the whole request was
    /// written to it still expects the rest of the body: `end_stream` must not
    /// pool it, or the next request would be read as that body
    /// (sozu-proxy/sozu#1721). The same connection, once the request is
    /// complete too, is pooled: that half pins the guard is not a blanket
    /// refusal.
    ///
    /// A request received whole but not yet fully written upstream is just
    /// as incomplete for that backend.
    ///
    /// TO SEE THIS RED: in `ConnectionH1::end_stream`, drop the two
    /// `stream.front` conditions from the `BackendStatus::Connected` arm. The
    /// incomplete request then leaves the connection `KeepAlive`.
    #[test]
    fn a_backend_that_answered_before_the_whole_request_is_not_pooled() {
        const BACKEND: mio::Token = mio::Token(1);
        // (request phase, request bytes still unwritten, pooled?)
        for (request_phase, unwritten, expect_pooled) in [
            (kawa::ParsingPhase::Body, false, false),
            (kawa::ParsingPhase::Chunks { first: false }, false, false),
            (kawa::ParsingPhase::Terminated, true, false),
            (kawa::ParsingPhase::Terminated, false, true),
        ] {
            let pool = Rc::new(RefCell::new(Pool::with_capacity(4, 8, 16384)));
            let mut context = test_context(&pool);
            context
                .create_stream(Ulid::generate(), 1 << 16)
                .expect("the test pool must hand out stream buffers");
            let (back_socket, _back_peer) = connected_socket();
            let backend_ulid = Ulid::generate();
            let mut client = h1_of(Connection::new_h1_client(
                backend_ulid,
                SessionTcpStream::new(back_socket, backend_ulid, None),
                "test-cluster".into(),
                test_backend_id(cached_peer()),
                Duration::from_secs(60),
            ));
            if let Position::Client(_, _, status) = &mut client.position {
                *status = BackendStatus::Connected;
            }
            client.stream = Some(0);
            let stream = &mut context.streams[0];
            stream.state = StreamState::Linked(BACKEND);
            stream.context.keep_alive_backend = true;
            // The response is complete; the request is as staged.
            stream.back.parsing_phase = kawa::ParsingPhase::Terminated;
            stream.front.parsing_phase = request_phase;
            if unwritten {
                // Received whole, but its end was never written upstream.
                stream
                    .front
                    .blocks
                    .push_back(kawa::Block::Flags(kawa::Flags {
                        end_body: true,
                        end_chunk: false,
                        end_header: false,
                        end_stream: true,
                    }));
            }

            client.end_stream(0, &mut context);

            let pooled = matches!(
                client.position,
                Position::Client(_, _, BackendStatus::KeepAlive)
            );
            assert_eq!(
                pooled,
                expect_pooled,
                "a request in {request_phase:?} (unwritten bytes: {unwritten}) must {}pool the backend connection",
                if expect_pooled { "" } else { "not " }
            );
        }
    }

    /// A frontend socket whose `close_notify` is left pending by the close and
    /// flushed by the next write, as a TLS socket whose peer had not drained
    /// it yet: the close is deferred, and `writable` runs again on the same
    /// connection once the flush went through.
    #[derive(Debug)]
    struct PendingCloseNotifySocket {
        stream: mio::net::TcpStream,
        closing: bool,
    }

    impl SocketHandler for PendingCloseNotifySocket {
        fn socket_read(&mut self, buf: &mut [u8]) -> (usize, SocketResult) {
            self.stream.socket_read(buf)
        }

        fn socket_write(&mut self, buf: &[u8]) -> (usize, SocketResult) {
            self.stream.socket_write(buf)
        }

        fn socket_write_vectored(&mut self, bufs: &[IoSlice]) -> (usize, SocketResult) {
            // The pending `close_notify` leaves with this write.
            self.closing = false;
            self.stream.socket_write_vectored(bufs)
        }

        fn socket_wants_write(&self) -> bool {
            self.closing
        }

        fn socket_close(&mut self) {
            self.closing = true;
        }

        fn socket_ref(&self) -> &mio::net::TcpStream {
            &self.stream
        }

        fn socket_mut(&mut self) -> &mut mio::net::TcpStream {
            &mut self.stream
        }

        fn peer_addr(&self) -> Option<std::net::SocketAddr> {
            self.stream.peer_addr().ok()
        }

        fn protocol(&self) -> crate::socket::TransportProtocol {
            crate::socket::TransportProtocol::Tcp
        }

        fn read_error(&self) {}

        fn write_error(&self) {}
    }

    /// A frontend socket that still reports buffered TLS records and answers
    /// every write with `status`, as a TLS socket whose peer stopped reading
    /// (`WouldBlock`) or whose write met a socket error (`Error`).
    #[derive(Debug)]
    struct StuckTlsSocket {
        stream: mio::net::TcpStream,
        status: SocketResult,
    }

    impl SocketHandler for StuckTlsSocket {
        fn socket_read(&mut self, buf: &mut [u8]) -> (usize, SocketResult) {
            self.stream.socket_read(buf)
        }

        fn socket_write(&mut self, _buf: &[u8]) -> (usize, SocketResult) {
            (0, self.status)
        }

        fn socket_write_vectored(&mut self, _bufs: &[IoSlice]) -> (usize, SocketResult) {
            (0, self.status)
        }

        fn socket_wants_write(&self) -> bool {
            true
        }

        fn socket_ref(&self) -> &mio::net::TcpStream {
            &self.stream
        }

        fn socket_mut(&mut self) -> &mut mio::net::TcpStream {
            &mut self.stream
        }

        fn peer_addr(&self) -> Option<std::net::SocketAddr> {
            self.stream.peer_addr().ok()
        }

        fn protocol(&self) -> crate::socket::TransportProtocol {
            crate::socket::TransportProtocol::Tls1_3
        }

        fn read_error(&self) {}

        fn write_error(&self) {}
    }

    /// A TLS flush that did not answer `Continue` leaves WRITABLE to the next
    /// kernel edge, even while the socket still reports records: re-raising
    /// it would make `Mux::ready_inner` retry the same refused or failed
    /// write on every inner iteration until `MAX_LOOP_ITERATIONS`
    /// (sozu-proxy/sozu#1780).
    ///
    /// Both flushes are covered: the one with no stream, and the TLS-only
    /// flush of a stream with nothing left to write.
    ///
    /// On this server-side connection an `Error` flush instead closes the
    /// session through `ConnectionH1::client_write_failed`, before that check
    /// (sozu-proxy/sozu#1779): it stops reporting pending output, and leaves
    /// WRITABLE unraised too.
    ///
    /// TO SEE THIS RED: in either flush of `ConnectionH1::writable`, drop the
    /// `status == SocketResult::Continue` condition. The `WouldBlock` case
    /// then re-raises WRITABLE.
    #[test]
    fn a_tls_flush_that_did_not_continue_leaves_writable_to_the_kernel() {
        for (status, with_stream) in [
            (SocketResult::WouldBlock, false),
            (SocketResult::Error, false),
            (SocketResult::WouldBlock, true),
            (SocketResult::Error, true),
        ] {
            let pool = Rc::new(RefCell::new(Pool::with_capacity(2, 4, 16384)));
            let mut context = test_context(&pool);
            if with_stream {
                context
                    .create_stream(Ulid::generate(), 1 << 16)
                    .expect("the test pool must hand out stream buffers");
            }
            let (socket, _peer) = connected_socket();
            let mut frontend = Connection::new_h1_server(
                Ulid::generate(),
                StuckTlsSocket {
                    stream: socket,
                    status,
                },
                Duration::from_secs(60),
            );
            if let Connection::H1(server) = &mut frontend {
                server.stream = with_stream.then_some(0);
            }
            let mut router = Router::new(Duration::from_secs(10), Duration::from_secs(10));
            frontend.readiness_mut().interest.insert(Ready::WRITABLE);
            frontend.readiness_mut().event.insert(Ready::WRITABLE);

            let result = frontend.writable(&mut context, EndpointClient(&mut router));

            if status == SocketResult::Error {
                assert!(
                    matches!(result, MuxResult::CloseSession),
                    "a failed flush (stream: {with_stream}) must close the session"
                );
                assert!(
                    !frontend.has_pending_write(),
                    "a failed flush (stream: {with_stream}) must stop reporting output"
                );
            } else {
                assert!(matches!(result, MuxResult::Continue));
                assert!(
                    frontend.has_pending_write(),
                    "premise: the socket must still report records"
                );
            }
            assert!(
                !frontend.readiness().event.is_writable(),
                "a flush that answered {status:?} (stream: {with_stream}) must not re-raise \
                 WRITABLE, got {:?}",
                frontend.readiness()
            );
        }
    }

    /// Once `writable` decided to close after a response, a deferred TLS
    /// close must not run the completion again: the next pass would log the
    /// request a second time, count it twice, and re-take the keep-alive
    /// decision after `close_notify` — keeping the connection if the body
    /// finished in between (sozu-proxy/sozu#1721).
    ///
    /// Once `close_notify` is flushed, the request still incomplete, the
    /// connection starts its lingering close (RFC 9112 §9.6) and closes the
    /// session once the client closes.
    ///
    /// TO SEE THIS RED: in `ConnectionH1::writable`, delete the
    /// `stream.context.closing = true;` of the close path. The second pass
    /// then logs a second `H1::Complete` line and resets the stream to
    /// `Idle` instead of closing the session.
    #[test]
    fn a_deferred_tls_close_completes_the_response_once() {
        const BACKEND: mio::Token = mio::Token(1);
        let (states_sender, states_receiver) = std::sync::mpsc::channel();

        let output = crate::capture_test_logs(move || {
            let pool = Rc::new(RefCell::new(Pool::with_capacity(4, 8, 16384)));
            let mut context = test_context(&pool);
            context
                .create_stream(Ulid::generate(), 1 << 16)
                .expect("the test pool must hand out stream buffers");
            let (front_socket, mut front_peer) = connected_socket();
            front_peer
                .set_nonblocking(true)
                .expect("the frontend peer is drained without blocking");
            let mut frontend = Connection::new_h1_server(
                Ulid::generate(),
                PendingCloseNotifySocket {
                    stream: front_socket,
                    closing: false,
                },
                Duration::from_secs(60),
            );
            if let Connection::H1(server) = &mut frontend {
                server.stream = Some(0);
            }
            let (back_socket, mut back_peer) = connected_socket();
            let backend_ulid = Ulid::generate();
            let mut client = h1_of(Connection::new_h1_client(
                backend_ulid,
                SessionTcpStream::new(back_socket, backend_ulid, None),
                "test-cluster".into(),
                test_backend_id(cached_peer()),
                Duration::from_secs(60),
            ));
            if let Position::Client(_, _, status) = &mut client.position {
                *status = BackendStatus::KeepAlive;
            }
            assert!(
                client.start_stream(0, &mut context),
                "premise: the backend must accept the request"
            );
            let stream = &mut context.streams[0];
            stream.state = StreamState::Linked(BACKEND);
            stream.context.keep_alive_frontend = true;
            // An upload still in its body: the response completes first.
            stream.front.parsing_phase = kawa::ParsingPhase::Body;
            stream.metrics.service_start();
            stream.metrics.backend_id = Some("test-backend".into());
            stream.metrics.backend_start();
            stream.metrics.backend_connected();

            back_peer
                .write_all(b"HTTP/1.1 413 Content Too Large\r\nContent-Length: 0\r\n\r\n")
                .expect("the loopback backend peer must accept the response");
            for _ in 0..64 {
                client.readiness.event.insert(Ready::READABLE);
                client.readable(&mut context, EndpointServer(&mut frontend));
                if context.streams[0].back.is_terminated() {
                    break;
                }
            }
            assert!(
                context.streams[0].back.is_terminated(),
                "premise: the early response must be read whole"
            );
            let mut router = Router::new(Duration::from_secs(10), Duration::from_secs(10));
            router.backends.insert(BACKEND, Connection::H1(client));

            let mut states = Vec::new();
            for pass in 0..2 {
                if pass == 1 {
                    // The body finishes after the close began.
                    context.streams[0].front.parsing_phase = kawa::ParsingPhase::Terminated;
                }
                frontend.readiness_mut().event.insert(Ready::WRITABLE);
                let result = frontend.writable(&mut context, EndpointClient(&mut router));
                states.push((result, context.streams[0].state));
                if pass == 0 {
                    assert_eq!(
                        context.streams[0].state,
                        StreamState::Unlinked,
                        "premise: the first pass completes the response and closes"
                    );
                }
            }
            let Connection::H1(server) = &mut frontend else {
                unreachable!("the frontend is H1");
            };
            let lingering = server.is_lingering();
            let peer_eof = read_to_eof(&mut front_peer).is_some();
            // The client closes: the drain meets its EOF and the session closes.
            drop(front_peer);
            let mut after_client_close = MuxResult::Continue;
            for _ in 0..1000 {
                server.readiness.event.insert(Ready::READABLE);
                after_client_close = server.readable(&mut context, EndpointClient(&mut router));
                if matches!(after_client_close, MuxResult::CloseSession) {
                    break;
                }
                std::thread::sleep(Duration::from_millis(1));
            }
            states_sender
                .send((states, lingering, peer_eof, after_client_close))
                .expect("the test thread must report its states");
        });

        let (states, lingering, peer_eof, after_client_close) = states_receiver
            .recv()
            .expect("the test thread must report its states");
        let completions = output
            .lines()
            .filter(|line| line.contains("H1::Complete"))
            .count();
        assert_eq!(
            completions, 1,
            "the response must be completed and logged once, got: {output}"
        );
        for (pass, (_, state)) in states.iter().enumerate() {
            assert_ne!(
                *state,
                StreamState::Idle,
                "pass {pass}: no keep-alive reset after the close began"
            );
        }
        assert!(
            matches!(states[0].0, MuxResult::Continue),
            "the first pass defers the close behind the pending close_notify"
        );
        assert!(
            matches!(states[1].0, MuxResult::Continue) && lingering,
            "once close_notify is flushed the connection lingers, got {:?}",
            states[1].0
        );
        assert!(
            peer_eof,
            "the lingering close sends the FIN after the response"
        );
        assert!(
            matches!(after_client_close, MuxResult::CloseSession),
            "the session closes once the client closed, got {after_client_close:?}"
        );
    }

    /// Read `peer` until its EOF, retrying a nonblocking read for up to about
    /// one second. `None` when no EOF arrived in that time.
    fn read_to_eof(peer: &mut std::net::TcpStream) -> Option<Vec<u8>> {
        let mut received = Vec::new();
        let mut buf = [0u8; 4096];
        for _ in 0..1000 {
            match std::io::Read::read(peer, &mut buf) {
                Ok(0) => return Some(received),
                Ok(size) => received.extend_from_slice(&buf[..size]),
                Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => {
                    std::thread::sleep(Duration::from_millis(1));
                }
                Err(_) => return None,
            }
        }
        None
    }

    /// A frontend connection lingering over a live loopback socket, and the
    /// client end of that socket.
    fn lingering_frontend(
        deadline: Instant,
    ) -> (ConnectionH1<SessionTcpStream>, std::net::TcpStream) {
        let (front_socket, front_peer) = connected_socket();
        let ulid = Ulid::generate();
        let mut server = h1_of(Connection::new_h1_server(
            ulid,
            SessionTcpStream::new(front_socket, ulid, None),
            Duration::from_secs(10),
        ));
        server.linger = Linger::Pending { deadline };
        assert!(
            matches!(
                server.defer_close_for_tls_flush("test"),
                MuxResult::Continue
            ),
            "a pending linger starts instead of closing"
        );
        (server, front_peer)
    }

    /// Drive `readable` until it closes the session or reads everything
    /// queued. Returns the last result.
    fn drain_passes<L: ListenerHandler + L7ListenerHandler>(
        server: &mut ConnectionH1<SessionTcpStream>,
        context: &mut Context<L>,
        router: &mut Router,
    ) -> MuxResult {
        let mut result = MuxResult::Continue;
        for _ in 0..100 {
            server.readiness.event.insert(Ready::READABLE);
            result = server.readable(context, EndpointClient(router));
            if matches!(result, MuxResult::CloseSession) {
                break;
            }
            std::thread::sleep(Duration::from_millis(1));
        }
        result
    }

    /// RFC 9112 §9.6 lingering close: the write side is shut down first, the
    /// rest of the request is read and dropped without pushing the deadline
    /// out, and the drain ends on the client's EOF.
    ///
    /// TO SEE THIS RED: in `ConnectionH1::readable`, move the
    /// `if self.is_lingering()` early return after `self.arm_timeout(...)`:
    /// the deadline moves with each read.
    #[test]
    fn a_lingering_close_half_closes_then_drains_to_the_client_eof() {
        let pool = Rc::new(RefCell::new(Pool::with_capacity(4, 8, 16384)));
        let mut context = test_context(&pool);
        context
            .create_stream(Ulid::generate(), 1 << 16)
            .expect("the test pool must hand out stream buffers");
        let mut router = Router::new(Duration::from_secs(10), Duration::from_secs(10));
        let deadline = Instant::now() + Duration::from_secs(10);
        let (mut server, mut front_peer) = lingering_frontend(deadline);

        assert!(server.is_lingering(), "the connection drains");
        assert_eq!(
            server.poll_timeout(),
            Some(deadline),
            "the drain is bounded"
        );
        assert!(
            !server.readiness.interest.is_writable(),
            "nothing is written while lingering"
        );
        assert_eq!(
            read_to_eof(&mut front_peer),
            Some(Vec::new()),
            "the client sees the FIN while it may still send"
        );

        // The client keeps sending: sozu drops it, and the deadline stays.
        front_peer
            .write_all(&vec![b'y'; 100 * 1024])
            .expect("the client may still send after the FIN");
        context.now = Instant::now();
        let result = drain_passes(&mut server, &mut context, &mut router);
        assert!(
            matches!(result, MuxResult::Continue),
            "the drain waits for more, got {result:?}"
        );
        match server.linger {
            Linger::Draining { remaining, .. } => assert_eq!(
                remaining,
                LINGER_MAX_BYTES - 100 * 1024,
                "every byte sent is drained and counted"
            ),
            other => panic!("the connection must still drain, got {other:?}"),
        }
        assert_eq!(
            server.poll_timeout(),
            Some(deadline),
            "a drained read never pushes the deadline out"
        );

        drop(front_peer);
        let result = drain_passes(&mut server, &mut context, &mut router);
        assert!(
            matches!(result, MuxResult::CloseSession),
            "the client's EOF ends the drain, got {result:?}"
        );
    }

    /// The drain is bounded in bytes and in time: past either bound the
    /// session closes although the client is still sending.
    ///
    /// TO SEE THIS RED: in `ConnectionH1::drain_linger`, read `buf.len()`
    /// bytes instead of `remaining.min(buf.len())` (the budget case reads
    /// past its budget), or delete the `now >= deadline` check (the deadline
    /// case keeps draining).
    #[test]
    fn a_lingering_close_stops_at_its_byte_budget_and_its_deadline() {
        let pool = Rc::new(RefCell::new(Pool::with_capacity(4, 8, 16384)));
        let mut context = test_context(&pool);
        context
            .create_stream(Ulid::generate(), 1 << 16)
            .expect("the test pool must hand out stream buffers");
        let mut router = Router::new(Duration::from_secs(10), Duration::from_secs(10));

        // Byte budget: the client sends more than is left of it.
        let deadline = Instant::now() + Duration::from_secs(10);
        let (mut server, mut front_peer) = lingering_frontend(deadline);
        server.linger = Linger::Draining {
            deadline,
            remaining: 1024,
        };
        front_peer
            .write_all(&[b'y'; 4096])
            .expect("the client may still send after the FIN");
        context.now = Instant::now();
        let result = drain_passes(&mut server, &mut context, &mut router);
        assert!(
            matches!(result, MuxResult::CloseSession),
            "past its byte budget the drain closes, got {result:?}"
        );
        assert_eq!(
            server.linger,
            Linger::Draining {
                deadline,
                remaining: 0
            },
            "the drain read exactly its budget"
        );

        // Deadline: the client still sends when it passes.
        let (mut server, mut front_peer) = lingering_frontend(deadline);
        front_peer
            .write_all(&[b'y'; 4096])
            .expect("the client may still send after the FIN");
        context.now = deadline;
        server.readiness.event.insert(Ready::READABLE);
        let result = server.readable(&mut context, EndpointClient(&mut router));
        assert!(
            matches!(result, MuxResult::CloseSession),
            "past its deadline the drain closes, got {result:?}"
        );
    }

    /// A header the parser refuses, after a well-formed request line: the
    /// access log must still name the method and the path.
    ///
    /// The authority is asserted ABSENT, on purpose. This request carries a
    /// `Host` line before the bad one, so a `Host` block exists here — but a
    /// client that puts the bad line first gets no `Host` block at all, so
    /// reading it would log an unvalidated value whenever the client chose
    /// to. Only an authority the request line itself carries is logged.
    ///
    /// To SEE THIS RED: in `Stream::generate_access_log`
    /// (`lib/src/protocol/mux/stream.rs`), replace the
    /// `rejected_request_line(&self.front)` call with
    /// `None::<RejectedRequestLine<'_>>`. The line then renders
    /// `- - - 400`.
    #[test]
    fn a_header_parse_error_keeps_the_method_and_path_in_the_access_log() {
        let output = access_log_of_a_rejected_h1_request(
            b"GET /diag?x=1 HTTP/1.1\r\nHost: example.com\r\nBad Header: x\r\n\r\n",
        );

        assert!(
            output.contains("GET /diag?x=1 400"),
            "the access log of a request rejected on a header must carry its \
             method and path, got: {output}"
        );
        assert!(
            output.contains("- GET /diag?x=1 400"),
            "the authority of an origin-form request rejected on a header must \
             not be read from a `Host` line, got: {output}"
        );
        assert!(
            output.contains("H1::Complete"),
            "the 400 of a parse error is logged by `ConnectionH1::writable` once \
             the answer is written, not at session close, got: {output}"
        );
    }

    /// Sōzu's own CL.TE guard in `HttpContext::on_request_headers`
    /// (`lib/src/protocol/kawa_h1/editor.rs`) rejects a request kawa parsed
    /// cleanly, and returns before the request line is captured. Here kawa's
    /// `process_headers` already resolved the authority and the path, so all
    /// three fields are logged.
    ///
    /// `identity` then `chunked` is the pair that reaches that guard: kawa
    /// combines it to a chunked-final coding and accepts it, and the guard
    /// refuses to forward two TE lines. A lone `Transfer-Encoding: gzip`
    /// would not — kawa refuses it itself, before resolving the authority.
    ///
    /// To SEE THIS RED: the same substitution as
    /// `a_header_parse_error_keeps_the_method_and_path_in_the_access_log`.
    #[test]
    fn a_cl_te_rejection_keeps_the_whole_request_line_in_the_access_log() {
        let output = access_log_of_a_rejected_h1_request(
            b"POST /upload HTTP/1.1\r\nHost: example.com\r\n\
              Transfer-Encoding: identity\r\nTransfer-Encoding: chunked\r\n\r\n",
        );

        assert!(
            output.contains("example.com POST /upload 400"),
            "the access log of a request rejected by the CL.TE guard must carry \
             its authority, method and path, got: {output}"
        );
    }

    /// The one authority a header rejection may log: the one the request
    /// line itself carries. An absolute-form target names it, so it is
    /// logged, split from the path the way kawa splits it.
    ///
    /// To SEE THIS RED: the same substitution as
    /// `a_header_parse_error_keeps_the_method_and_path_in_the_access_log`.
    #[test]
    fn a_header_parse_error_logs_the_authority_an_absolute_form_carries() {
        let output = access_log_of_a_rejected_h1_request(
            b"GET http://example.com:8080/abs HTTP/1.1\r\nBad Header: x\r\n\r\n",
        );

        assert!(
            output.contains("example.com:8080 GET /abs 400"),
            "the access log of an absolute-form request rejected on a header \
             must carry the authority of its request-target, got: {output}"
        );
    }

    // ── A warm H1 write pass allocates nothing ───────────────────────────
    //
    // DO NOT READ THESE TESTS AS COVERING H2. `H2Shell::write_streams`
    // (`lib/src/protocol/mux/h2.rs`) is a separate write path with its own
    // allocation test.

    /// `Store` blocks each measured pass queues. A fresh `Vec<IoSlice>`
    /// reserves four descriptors on its first push, so a pass over more than
    /// four blocks also exercises the growth a per-pass vector would pay.
    /// One H1 header block is already dozens of them: the `BlockConverter`
    /// emits every name, separator, value and CRLF as its own `Store`.
    const WARM_PASS_BLOCKS: usize = 16;

    /// Queue [`WARM_PASS_BLOCKS`] blocks on `kawa`, drive two write passes
    /// through `write`, and return the heap allocations each one made.
    ///
    /// The first pass is the cold one of a fresh connection and may grow
    /// whatever the connection keeps; the second queues the same blocks again
    /// and is the warm measurement.
    /// Both go to a live loopback peer, so the write is a real `writev(2)`.
    /// The blocks are `Store::Static`, so queueing them touches no storage
    /// and the kawa never terminates: the pass stops after the write, before
    /// the completion arm and its access log, which is not the hot path.
    fn allocations_of_two_write_passes(
        context: &mut Context<crate::protocol::mux::test_support::TestListener>,
        outgoing: fn(
            &mut crate::protocol::mux::Stream,
        ) -> &mut crate::protocol::mux::GenericHttpStream,
        mut write: impl FnMut(&mut Context<crate::protocol::mux::test_support::TestListener>),
    ) -> [usize; 2] {
        let mut measured = [0; 2];
        for (pass, slot) in measured.iter_mut().enumerate() {
            let kawa = outgoing(&mut context.streams[0]);
            for _ in 0..WARM_PASS_BLOCKS {
                kawa.out
                    .push_back(kawa::OutBlock::Store(kawa::Store::Static(
                        b"x-warm: pass\r\n",
                    )));
            }
            let before = crate::test_allocations::allocations();
            write(context);
            let allocations = crate::test_allocations::allocations() - before;
            assert!(
                outgoing(&mut context.streams[0]).out.is_empty(),
                "premise: write pass {pass} must drain every queued block, or \
                 the measured pass would start from a different state"
            );
            *slot = allocations;
        }
        measured
    }

    /// The frontend side: a `Position::Server` pass writing the response.
    ///
    /// TO SEE THIS RED: in `ConnectionH1::writable`, declare
    /// `let mut io_slices: Vec<IoSlice<'static>> = Vec::new();` above the
    /// `gather` call and hand that local to `gather`, the emptiness checks,
    /// the write, the replay capture and `confirm` in place of
    /// `self.io_slices`. Measured with 16 blocks: `left: 3, right: 0` under
    /// both `cargo test` and `cargo test --release` — one allocation and two
    /// `realloc`s per pass, as the per-pass vector grows 4 → 8 → 16.
    #[test]
    fn a_warm_h1_response_write_pass_allocates_nothing() {
        let mut fixture = linked_backend_connection();
        let BackendReadFixture {
            context, frontend, ..
        } = &mut fixture;
        let Connection::H1(server) = frontend else {
            unreachable!("new_h1_server builds an H1 connection");
        };
        server.stream = Some(0);
        let mut router = Router::new(Duration::from_secs(10), Duration::from_secs(10));

        let [_, warm] = allocations_of_two_write_passes(
            context,
            |stream| &mut stream.back,
            |context| {
                server.readiness.event.insert(Ready::WRITABLE);
                server.writable(context, EndpointClient(&mut router));
            },
        );

        assert_eq!(warm, 0, "a warm H1 response write pass must not allocate");
    }

    /// The backend side: a `Position::Client` pass writing the request.
    ///
    /// The fixture's connection was dialled fresh, not taken from the
    /// keep-alive pool, so the replay capture stays off: its `reserve` is the
    /// one allocation a pooled upstream is documented to pay, and it is not
    /// what this pins.
    ///
    /// TO SEE THIS RED: the same substitution as
    /// `a_warm_h1_response_write_pass_allocates_nothing`.
    #[test]
    fn a_warm_h1_request_write_pass_allocates_nothing() {
        let mut fixture = linked_backend_connection();
        let BackendReadFixture {
            context,
            frontend,
            client,
            ..
        } = &mut fixture;
        assert!(
            !client.reused_from_pool,
            "premise: a freshly dialled upstream does not capture for replay"
        );

        let [_, warm] = allocations_of_two_write_passes(
            context,
            |stream| &mut stream.front,
            |context| {
                client.readiness.event.insert(Ready::WRITABLE);
                client.writable(context, EndpointServer(frontend));
            },
        );

        assert_eq!(warm, 0, "a warm H1 request write pass must not allocate");
    }

    /// #1610: an H1 frontend sizes its session's link queue for the one
    /// request it has in flight, not the four a first `push_back` reserves.
    ///
    /// Measured against a control whose queue already holds room for one
    /// link, so the difference is the queue's own allocation and nothing else
    /// the parse allocates.
    ///
    /// TO SEE THIS RED: drop the `reserve_exact(1)` before the
    /// `context.pending_links.push_back(stream_id)` that queues a parsed
    /// request in `ConnectionH1::readable`. The queue then allocates four
    /// slots.
    #[test]
    fn a_parsed_h1_request_queues_its_link_in_a_queue_sized_for_one() {
        use crate::test_allocations::bytes;

        let parse = |presized: bool| {
            let pool = Rc::new(RefCell::new(Pool::with_capacity(4, 8, 16384)));
            let mut context = test_context(&pool);
            context
                .create_stream(Ulid::generate(), 1 << 16)
                .expect("the test pool must hand out stream buffers");
            if presized {
                context.pending_links.reserve_exact(1);
            }
            let (front_socket, mut front_peer) = connected_socket();
            let mut server = h1_of(Connection::new_h1_server(
                Ulid::generate(),
                front_socket,
                Duration::from_secs(60),
            ));
            server.stream = Some(0);
            let mut router = Router::new(Duration::from_secs(10), Duration::from_secs(10));
            front_peer
                .write_all(b"GET / HTTP/1.1\r\nHost: example.com\r\n\r\n")
                .expect("the loopback peer must accept the staged request bytes");

            let before = bytes();
            for _ in 0..64 {
                server.readiness.event.insert(Ready::READABLE);
                server.readable(&mut context, EndpointClient(&mut router));
                if !context.pending_links.is_empty() {
                    break;
                }
            }
            let parsed = bytes() - before;
            assert_eq!(
                context.pending_links,
                [0],
                "premise: the parsed request must be queued for linking"
            );
            parsed
        };

        // The thread's first parse also initialises state that outlives the
        // session; run one before measuring.
        let _ = parse(true);
        assert_eq!(
            parse(false) - parse(true),
            std::mem::size_of::<GlobalStreamId>(),
            "an H1 frontend must queue its link in a queue sized for one"
        );
    }

    /// #1610: the first write pass of a fresh connection sizes its descriptor
    /// vector in ONE allocation, on both sides.
    ///
    /// Measured against a control that is the same pass on a connection whose
    /// `io_slices` already holds [`WARM_PASS_BLOCKS`] descriptors, so the
    /// difference is the vector's own cost and nothing else the first pass
    /// sets up (which differs between debug and release builds).
    ///
    /// TO SEE THIS RED: drop the `io_slices.reserve(kawa.out.len())` in
    /// `h2_transmit::gather`. The fresh vector then doubles 4 → 8 → 16 over the
    /// [`WARM_PASS_BLOCKS`] blocks: three allocations per side.
    #[test]
    fn a_cold_h1_write_pass_sizes_its_descriptor_vector_once() {
        let response = |presized: bool| {
            let mut fixture = linked_backend_connection();
            let BackendReadFixture {
                context, frontend, ..
            } = &mut fixture;
            let Connection::H1(server) = frontend else {
                unreachable!("new_h1_server builds an H1 connection");
            };
            if presized {
                server.io_slices.reserve(WARM_PASS_BLOCKS);
            }
            server.stream = Some(0);
            let mut router = Router::new(Duration::from_secs(10), Duration::from_secs(10));
            allocations_of_two_write_passes(
                context,
                |stream| &mut stream.back,
                |context| {
                    server.readiness.event.insert(Ready::WRITABLE);
                    server.writable(context, EndpointClient(&mut router));
                },
            )[0]
        };
        let request = |presized: bool| {
            let mut fixture = linked_backend_connection();
            let BackendReadFixture {
                context,
                frontend,
                client,
                ..
            } = &mut fixture;
            if presized {
                client.io_slices.reserve(WARM_PASS_BLOCKS);
            }
            allocations_of_two_write_passes(
                context,
                |stream| &mut stream.front,
                |context| {
                    client.readiness.event.insert(Ready::WRITABLE);
                    client.writable(context, EndpointServer(frontend));
                },
            )[0]
        };

        // The thread's first pass also initialises state that outlives the
        // connection; run one of each before measuring.
        let _ = (response(true), request(true));
        assert_eq!(
            response(false) - response(true),
            1,
            "a fresh H1 frontend must size its descriptor vector in one allocation"
        );
        assert_eq!(
            request(false) - request(true),
            1,
            "a fresh H1 backend must size its descriptor vector in one allocation"
        );
    }

    /// What kawa's H1 serializer writes for a `kind` message framed by
    /// `Content-Length: 5`, holding `hello`, after `pkawa::handle_trailer`
    /// queued an H2 trailer block of `fields` and the H1 write path ran
    /// `drop_length_framed_trailers`, which returns whether it dropped one.
    fn h1_bytes_of_a_length_framed_message(
        kind: kawa::Kind,
        fields: &[(&[u8], &[u8])],
    ) -> (bool, String) {
        let mut pool = Pool::with_capacity(1, 1, 4096);
        let checkout = pool
            .checkout()
            .expect("the test pool must hand out a buffer");
        let mut kawa: super::super::GenericHttpStream =
            kawa::Kawa::new(kind, kawa::Buffer::new(checkout));
        kawa.body_size = kawa::BodySize::Length(5);
        // One DATA frame, as `handle_data` queues it for a length-framed
        // message.
        kawa.push_block(kawa::Block::Chunk(kawa::Chunk {
            data: kawa::Store::Static(b"hello"),
        }));
        let mut encoder = super::super::hpack::Encoder::new();
        let mut encoded = Vec::new();
        for &(name, value) in fields {
            encoder.encode_header_into((name, value), &mut encoded);
        }
        let result = super::super::pkawa::handle_trailer(
            &mut kawa,
            &encoded,
            true,
            &mut super::super::hpack::Decoder::new(),
            super::super::h2::MAX_HEADER_LIST_SIZE as u32,
            u32::MAX,
            false,
            &mut Vec::new(),
        );
        assert!(result.is_ok(), "handle_trailer failed: {:?}", result.err());

        let dropped = ConnectionH1::<mio::net::TcpStream>::drop_length_framed_trailers(&mut kawa);
        kawa.prepare(&mut kawa::h1::BlockConverter);
        let buffer = kawa.storage.buffer();
        let bytes: Vec<u8> = kawa
            .out
            .iter()
            .flat_map(|block| match block {
                kawa::OutBlock::Store(store) => store.data(buffer).to_vec(),
                kawa::OutBlock::Delimiter => Vec::new(),
            })
            .collect();
        (dropped, String::from_utf8_lossy(&bytes).into_owned())
    }

    /// An H2 trailer block on a Content-Length-framed message adds nothing
    /// after the body on HTTP/1.1, which carries trailers only with chunked
    /// coding (RFC 9112 §6.3, §7.1): the fields and the empty line are
    /// dropped, for a request towards an H1 backend and for a response
    /// towards an H1 client, with one trailer field or none.
    ///
    /// TO SEE THIS RED: make `drop_length_framed_trailers` return `false`
    /// before touching the block queue.
    #[test]
    fn a_length_framed_message_writes_no_trailer_to_an_h1_peer() {
        let with_field: &[(&[u8], &[u8])] = &[(b"grpc-status", b"0")];
        for kind in [kawa::Kind::Request, kawa::Kind::Response] {
            for fields in [with_field, &[]] {
                assert_eq!(
                    h1_bytes_of_a_length_framed_message(kind, fields),
                    (true, "hello".to_owned()),
                    "{kind:?} with {} trailer field(s)",
                    fields.len()
                );
            }
        }
    }

    /// What `ConnectionH1::writable` writes to an H1 client for a response
    /// from an H2 backend whose header section `head` arrived without
    /// END_STREAM, ended by a trailer HEADERS block of `trailer` fields when
    /// `trailer` is `Some`, or by an empty DATA frame with END_STREAM
    /// otherwise. `body_size`, when `Some`, overrides the framing
    /// `pkawa::handle_header` resolved. Returns the resolved framing and the
    /// bytes, written in one pass like a backend that sent the whole
    /// response before the client side became writable.
    fn h1_bytes_of_a_bodiless_response(
        head: &[(&[u8], &[u8])],
        body_size: Option<kawa::BodySize>,
        trailer: Option<&[(&[u8], &[u8])]>,
    ) -> (kawa::BodySize, String) {
        struct NoCallbacks;
        impl kawa::h1::ParserCallbacks<crate::pool::Checkout> for NoCallbacks {
            fn on_headers(&mut self, _kawa: &mut super::super::GenericHttpStream) {}
        }
        let mut pool = Pool::with_capacity(1, 1, 4096);
        let checkout = pool
            .checkout()
            .expect("the test pool must hand out a buffer");
        let mut kawa: super::super::GenericHttpStream =
            kawa::Kawa::new(kawa::Kind::Response, kawa::Buffer::new(checkout));
        let mut decoder = super::super::hpack::Decoder::new();
        let mut encoder = super::super::hpack::Encoder::new();
        let mut encoded = Vec::new();
        for &(name, value) in head {
            encoder.encode_header_into((name, value), &mut encoded);
        }
        let (_, result) = super::super::pkawa::handle_header(
            &mut decoder,
            &mut crate::protocol::mux::h2_scheduler::Prioriser::default(),
            1,
            &mut kawa,
            &encoded,
            false,
            &mut NoCallbacks,
            super::super::h2::MAX_HEADER_LIST_SIZE as u32,
            u32::MAX,
            false,
        );
        assert!(result.is_ok(), "handle_header failed: {:?}", result.err());
        if let Some(body_size) = body_size {
            kawa.body_size = body_size;
        }
        match trailer {
            Some(fields) => {
                let mut encoded = Vec::new();
                for &(name, value) in fields {
                    encoder.encode_header_into((name, value), &mut encoded);
                }
                let result = super::super::pkawa::handle_trailer(
                    &mut kawa,
                    &encoded,
                    true,
                    &mut decoder,
                    super::super::h2::MAX_HEADER_LIST_SIZE as u32,
                    u32::MAX,
                    false,
                    &mut Vec::new(),
                );
                assert!(result.is_ok(), "handle_trailer failed: {:?}", result.err());
            }
            // The blocks `handle_data_frame` queues for an empty DATA frame
            // with END_STREAM.
            None => {
                kawa.push_block(kawa::Block::Chunk(kawa::Chunk {
                    data: kawa::Store::Static(b""),
                }));
                let end_chunk = kawa.is_streaming();
                kawa.push_block(kawa::Block::Flags(kawa::Flags {
                    end_body: true,
                    end_chunk,
                    end_header: false,
                    end_stream: true,
                }));
            }
        }
        let body_size = kawa.body_size;

        ConnectionH1::<mio::net::TcpStream>::drop_length_framed_trailers(&mut kawa);
        ConnectionH1::<mio::net::TcpStream>::drop_bodiless_response_framing(&mut kawa);
        kawa.prepare(&mut kawa::h1::BlockConverter);
        let buffer = kawa.storage.buffer();
        let bytes: Vec<u8> = kawa
            .out
            .iter()
            .flat_map(|block| match block {
                kawa::OutBlock::Store(store) => store.data(buffer).to_vec(),
                kawa::OutBlock::Delimiter => Vec::new(),
            })
            .collect();
        (body_size, String::from_utf8_lossy(&bytes).into_owned())
    }

    /// A response to HEAD, a 204 or a 304 ends with its header section on
    /// HTTP/1.1 (RFC 9112 §6.3 rule 1): whatever its framing, nothing
    /// follows the head towards an H1 client, neither a last chunk nor a
    /// trailer section (RFC 9110 §6.5.1), so the next response on a
    /// keep-alive connection starts right after it. Covers chunked framing,
    /// `Content-Length` framing and `BodySize::Empty`, each ended by a
    /// trailer block with one field or none, or by an empty DATA frame. A
    /// `:status 200` stands for the response to a HEAD: with no callbacks,
    /// `pkawa::handle_header` does not know the method and frames it chunked,
    /// with `Transfer-Encoding`, while it gives a 204 or a 304 no framing and
    /// removes a 204's `content-length` (RFC 9110 §8.6).
    ///
    /// TO SEE THIS RED: make `drop_bodiless_response_framing` return `false`
    /// before touching the block queue.
    #[test]
    fn a_bodiless_response_writes_nothing_after_its_head_to_an_h1_client() {
        let with_field: &[(&[u8], &[u8])] = &[(b"grpc-status", b"0")];
        for status in [&b"204"[..], b"304", b"200"] {
            let status_line = String::from_utf8_lossy(status);
            let (unframed_size, unframed) = if status == b"200" {
                (kawa::BodySize::Chunked, "Transfer-Encoding: chunked\r\n")
            } else {
                (kawa::BodySize::Empty, "")
            };
            let length_framed = |length: usize, field: &'static str| {
                if status == b"204" {
                    (kawa::BodySize::Empty, "")
                } else {
                    (kawa::BodySize::Length(length), field)
                }
            };
            let (zero_size, zero) = length_framed(0, "content-length: 0\r\n");
            let (five_size, five) = length_framed(5, "content-length: 5\r\n");
            for (content_length, body_size, expected_size, framing) in [
                (None, None, unframed_size, unframed),
                (
                    None,
                    Some(kawa::BodySize::Chunked),
                    kawa::BodySize::Chunked,
                    unframed,
                ),
                (Some(&b"0"[..]), None, zero_size, zero),
                (Some(b"5"), None, five_size, five),
                (
                    None,
                    Some(kawa::BodySize::Empty),
                    kawa::BodySize::Empty,
                    unframed,
                ),
            ] {
                let mut head: Vec<(&[u8], &[u8])> = vec![(b":status", status)];
                if let Some(length) = content_length {
                    head.push((b"content-length", length));
                }
                let expected = format!("HTTP/1.1 {status_line} FromH2\r\n{framing}\r\n");
                for trailer in [Some(with_field), Some(&[][..]), None] {
                    assert_eq!(
                        h1_bytes_of_a_bodiless_response(&head, body_size, trailer),
                        (expected_size, expected.clone()),
                        "{status_line} {expected_size:?} ended by {trailer:?}"
                    );
                }
            }
        }
    }

    /// The trailer block of a bodiless response queued alone, its header
    /// section written in an earlier pass, is dropped whole and counted;
    /// a queue with no trailer block reports none.
    #[test]
    fn a_bodiless_response_trailer_block_queued_after_its_head_is_dropped() {
        let mut pool = Pool::with_capacity(1, 1, 4096);
        let checkout = pool
            .checkout()
            .expect("the test pool must hand out a buffer");
        let mut kawa: super::super::GenericHttpStream =
            kawa::Kawa::new(kawa::Kind::Response, kawa::Buffer::new(checkout));
        kawa.body_size = kawa::BodySize::Chunked;
        let mut encoded = Vec::new();
        super::super::hpack::Encoder::new()
            .encode_header_into((b"grpc-status", b"0"), &mut encoded);
        let result = super::super::pkawa::handle_trailer(
            &mut kawa,
            &encoded,
            true,
            &mut super::super::hpack::Decoder::new(),
            super::super::h2::MAX_HEADER_LIST_SIZE as u32,
            u32::MAX,
            false,
            &mut Vec::new(),
        );
        assert!(result.is_ok(), "handle_trailer failed: {:?}", result.err());
        assert!(ConnectionH1::<mio::net::TcpStream>::drop_bodiless_response_framing(&mut kawa));
        assert!(
            !kawa
                .blocks
                .iter()
                .any(|block| matches!(block, kawa::Block::Header(_))),
            "the trailer fields are removed"
        );
        kawa.prepare(&mut kawa::h1::BlockConverter);
        assert!(kawa.out.is_empty(), "nothing is written after the head");
        assert!(!ConnectionH1::<mio::net::TcpStream>::drop_bodiless_response_framing(&mut kawa));
    }

    /// What kawa's H1 serializer writes to an H1 client for the header
    /// section `head` of an H2 backend response that arrived without
    /// END_STREAM, to a `method` request, and whether the response was then
    /// complete (`ParsingPhase::Terminated`).
    fn h1_head_of_an_h2_response(
        method: crate::protocol::kawa_h1::parser::Method,
        head: &[(&[u8], &[u8])],
    ) -> (String, bool) {
        let mut pool = Pool::with_capacity(1, 1, 4096);
        let checkout = pool
            .checkout()
            .expect("the test pool must hand out a buffer");
        let mut kawa: super::super::GenericHttpStream =
            kawa::Kawa::new(kawa::Kind::Response, kawa::Buffer::new(checkout));
        let mut context = test_http_context(Ulid::generate());
        context.method = Some(method);
        let mut encoded = Vec::new();
        let mut encoder = super::super::hpack::Encoder::new();
        for &(name, value) in head {
            encoder.encode_header_into((name, value), &mut encoded);
        }
        let (_, result) = super::super::pkawa::handle_header(
            &mut super::super::hpack::Decoder::new(),
            &mut crate::protocol::mux::h2_scheduler::Prioriser::default(),
            1,
            &mut kawa,
            &encoded,
            false,
            &mut context,
            super::super::h2::MAX_HEADER_LIST_SIZE as u32,
            u32::MAX,
            false,
        );
        assert!(result.is_ok(), "handle_header failed: {:?}", result.err());
        let terminated = kawa.is_terminated();
        kawa.prepare(&mut kawa::h1::BlockConverter);
        let buffer = kawa.storage.buffer();
        let bytes: Vec<u8> = kawa
            .out
            .iter()
            .flat_map(|block| match block {
                kawa::OutBlock::Store(store) => store.data(buffer).to_vec(),
                kawa::OutBlock::Delimiter => Vec::new(),
            })
            .collect();
        (String::from_utf8_lossy(&bytes).into_owned(), terminated)
    }

    /// An H2 response whose header section arrives without END_STREAM gains
    /// no `Transfer-Encoding` towards an H1 client when it has no content by
    /// definition (RFC 9110 §6.4.1): a 1xx or a 204 (RFC 9112 §6.1 MUST NOT),
    /// a 304 or a response to HEAD. A 1xx or a 204 also loses the
    /// `content-length` the backend sent, which a server MUST NOT send in
    /// them (RFC 9110 §8.6), while a 304 or a response to HEAD keeps it. A
    /// 1xx is complete at its head (`ParsingPhase::Terminated`), the final
    /// response following on the same stream; any other response waits for
    /// the END_STREAM of its stream (RFC 9113 §8.1). A `200` to GET still
    /// gains chunked framing.
    ///
    /// TO SEE THIS RED: drop the `no_content` guard of the chunked upgrade in
    /// `pkawa::handle_header`, or its removal of a 1xx or 204
    /// `content-length`.
    #[test]
    fn a_bodiless_h2_response_gains_no_transfer_encoding_towards_an_h1_client() {
        use crate::protocol::kawa_h1::parser::Method;
        /// The header fields of a response, `:status` first.
        type Head = &'static [(&'static [u8], &'static [u8])];
        let cases: [(Method, Head, &str, bool); 10] = [
            (
                Method::Get,
                &[(b":status", b"204")],
                "HTTP/1.1 204 FromH2\r\n",
                false,
            ),
            (
                Method::Get,
                &[(b":status", b"204"), (b"content-length", b"0")],
                "HTTP/1.1 204 FromH2\r\n",
                false,
            ),
            (
                Method::Get,
                &[(b":status", b"304")],
                "HTTP/1.1 304 FromH2\r\n",
                false,
            ),
            (
                Method::Get,
                &[(b":status", b"304"), (b"content-length", b"5")],
                "HTTP/1.1 304 FromH2\r\ncontent-length: 5\r\n",
                false,
            ),
            (
                Method::Get,
                &[(b":status", b"103"), (b"link", b"</a.css>")],
                "HTTP/1.1 103 FromH2\r\nlink: </a.css>\r\n",
                true,
            ),
            (
                Method::Get,
                &[
                    (b":status", b"103"),
                    (b"content-length", b"5"),
                    (b"link", b"</a.css>"),
                ],
                "HTTP/1.1 103 FromH2\r\nlink: </a.css>\r\n",
                true,
            ),
            (
                Method::Head,
                &[(b":status", b"200")],
                "HTTP/1.1 200 FromH2\r\n",
                false,
            ),
            (
                Method::Head,
                &[(b":status", b"200"), (b"content-length", b"5")],
                "HTTP/1.1 200 FromH2\r\ncontent-length: 5\r\n",
                false,
            ),
            (
                Method::Head,
                &[(b":status", b"200"), (b"content-length", b"0")],
                "HTTP/1.1 200 FromH2\r\ncontent-length: 0\r\n",
                false,
            ),
            (
                Method::Get,
                &[(b":status", b"200")],
                "HTTP/1.1 200 FromH2\r\nTransfer-Encoding: chunked\r\n",
                false,
            ),
        ];
        for (method, head, status_and_fields, terminated) in cases {
            let (bytes, complete) = h1_head_of_an_h2_response(method.clone(), head);
            let bytes = bytes
                .split("\r\n")
                .filter(|line| !line.starts_with("Sozu-Id: "))
                .collect::<Vec<_>>()
                .join("\r\n");
            assert_eq!(
                (bytes, complete),
                (format!("{status_and_fields}\r\n"), terminated),
                "{method:?} {head:?}"
            );
        }
    }

    /// Which responses `response_has_no_body` treats as bodiless.
    #[test]
    fn a_response_has_no_body_for_head_204_and_304_only() {
        use crate::protocol::kawa_h1::parser::Method;
        let cases = [
            (Some(Method::Head), Some(200), true),
            (Some(Method::Get), Some(204), true),
            (Some(Method::Get), Some(304), true),
            (Some(Method::Get), Some(200), false),
            (Some(Method::Post), Some(205), false),
            (None, None, false),
        ];
        for (method, status, expected) in cases {
            let mut context = test_http_context(Ulid::generate());
            context.method = method.clone();
            context.status = status;
            assert_eq!(
                ConnectionH1::<mio::net::TcpStream>::response_has_no_body(&context),
                expected,
                "{method:?} {status:?}"
            );
        }
    }

    /// The header section of a length-framed message closed with
    /// `end_stream` (a message with no body) is not a trailer block: it is
    /// written whole.
    #[test]
    fn a_length_framed_header_section_is_not_taken_for_trailers() {
        let mut pool = Pool::with_capacity(1, 1, 4096);
        let checkout = pool
            .checkout()
            .expect("the test pool must hand out a buffer");
        let mut kawa: super::super::GenericHttpStream =
            kawa::Kawa::new(kawa::Kind::Response, kawa::Buffer::new(checkout));
        kawa.body_size = kawa::BodySize::Length(0);
        kawa.push_block(kawa::Block::StatusLine);
        kawa.push_block(kawa::Block::Header(kawa::Pair {
            key: kawa::Store::Static(b"Content-Length"),
            val: kawa::Store::Static(b"0"),
        }));
        kawa.push_block(kawa::Block::Flags(kawa::Flags {
            end_body: false,
            end_chunk: false,
            end_header: true,
            end_stream: true,
        }));
        assert!(!ConnectionH1::<mio::net::TcpStream>::drop_length_framed_trailers(&mut kawa));
        assert_eq!(kawa.blocks.len(), 3, "the header section stays whole");
    }

    /// A `Cookies` block belongs to a header section: a length-framed
    /// request whose header section ends with its cookies and closes with
    /// `end_stream` is not a trailer block, and is written whole.
    ///
    /// TO SEE THIS RED: drop the `kawa::Block::Cookies` arm from the
    /// header-section guard of `drop_length_framed_trailers`.
    #[test]
    fn a_length_framed_header_section_ending_with_cookies_is_not_taken_for_trailers() {
        let mut pool = Pool::with_capacity(1, 1, 4096);
        let checkout = pool
            .checkout()
            .expect("the test pool must hand out a buffer");
        let mut kawa: super::super::GenericHttpStream =
            kawa::Kawa::new(kawa::Kind::Request, kawa::Buffer::new(checkout));
        kawa.body_size = kawa::BodySize::Length(0);
        kawa.push_block(kawa::Block::StatusLine);
        kawa.push_block(kawa::Block::Header(kawa::Pair {
            key: kawa::Store::Static(b"Content-Length"),
            val: kawa::Store::Static(b"0"),
        }));
        kawa.push_block(kawa::Block::Cookies);
        kawa.push_block(kawa::Block::Flags(kawa::Flags {
            end_body: false,
            end_chunk: false,
            end_header: true,
            end_stream: true,
        }));
        assert!(!ConnectionH1::<mio::net::TcpStream>::drop_length_framed_trailers(&mut kawa));
        assert_eq!(kawa.blocks.len(), 4, "the header section stays whole");
        assert!(
            matches!(
                kawa.blocks.back(),
                Some(kawa::Block::Flags(kawa::Flags {
                    end_header: true,
                    ..
                }))
            ),
            "the header section keeps its closing empty line"
        );
    }

    /// A response to HEAD declares the length of the body it does not carry
    /// (RFC 9110 §9.3.2): its header section alone ends the message with
    /// `end_stream`, under a non-zero `Content-Length`. It is not a trailer
    /// block, and is written whole.
    ///
    /// TO SEE THIS RED: drop the `kawa::Block::StatusLine` arm from the
    /// header-section guard of `drop_length_framed_trailers`.
    #[test]
    fn a_header_only_head_response_is_not_taken_for_trailers() {
        let mut pool = Pool::with_capacity(1, 1, 4096);
        let checkout = pool
            .checkout()
            .expect("the test pool must hand out a buffer");
        let mut kawa: super::super::GenericHttpStream =
            kawa::Kawa::new(kawa::Kind::Response, kawa::Buffer::new(checkout));
        kawa.body_size = kawa::BodySize::Length(5);
        kawa.push_block(kawa::Block::StatusLine);
        kawa.push_block(kawa::Block::Header(kawa::Pair {
            key: kawa::Store::Static(b"Content-Length"),
            val: kawa::Store::Static(b"5"),
        }));
        kawa.push_block(kawa::Block::Flags(kawa::Flags {
            end_body: false,
            end_chunk: false,
            end_header: true,
            end_stream: true,
        }));
        assert!(!ConnectionH1::<mio::net::TcpStream>::drop_length_framed_trailers(&mut kawa));
        assert_eq!(kawa.blocks.len(), 3, "the header section stays whole");
        assert!(
            matches!(
                kawa.blocks.back(),
                Some(kawa::Block::Flags(kawa::Flags {
                    end_header: true,
                    ..
                }))
            ),
            "the header section keeps its closing empty line"
        );
    }

    /// `ConnectionH1::terminate_close_delimited` at a backend EOF: only a body
    /// with neither `Content-Length` nor chunked coding is delimited by the
    /// close (RFC 9112 §6.3 rule 8). A `Content-Length` body still missing
    /// bytes (rule 5) and a chunked body without its terminating zero chunk
    /// (§7.1) are truncated and end in the Error phase, which the H2
    /// converter turns into RST_STREAM and the H1 frontend into a close.
    ///
    /// Red on `d5161919`: the `Content-Length` case ended `Terminated`
    /// (sozu-proxy/sozu#1633).
    #[test]
    fn only_a_close_delimited_body_ends_cleanly_at_the_backend_eof() {
        let cases: [(&[u8], bool); 3] = [
            (b"HTTP/1.1 200 OK\r\nContent-Length: 10\r\n\r\nabcd", true),
            (
                b"HTTP/1.1 200 OK\r\nTransfer-Encoding: chunked\r\n\r\n4\r\nabcd\r\n",
                true,
            ),
            (b"HTTP/1.1 200 OK\r\n\r\nabcd", false),
        ];
        for (bytes, truncated) in cases {
            let pool = Rc::new(RefCell::new(Pool::with_capacity(4, 8, 16384)));
            let mut context = test_context(&pool);
            let id = context
                .create_stream(Ulid::generate(), 1 << 16)
                .expect("the test pool must hand out stream buffers");
            let kawa = &mut context.streams[id].back;
            kawa.storage.space()[..bytes.len()].copy_from_slice(bytes);
            kawa.storage.fill(bytes.len());
            kawa::h1::parse(kawa, &mut kawa::h1::NoCallbacks);
            assert!(
                kawa.is_main_phase() && !kawa.is_terminated(),
                "premise: {:?} is in its body phase",
                kawa.body_size
            );
            ConnectionH1::<mio::net::TcpStream>::terminate_close_delimited(kawa, id);
            if truncated {
                assert!(
                    kawa.is_error(),
                    "a truncated {:?} body must end in Error, got {:?}",
                    kawa.body_size,
                    kawa.parsing_phase
                );
            } else {
                assert!(
                    kawa.is_terminated(),
                    "a close-delimited body ends cleanly, got {:?}",
                    kawa.parsing_phase
                );
            }
        }
    }
}
