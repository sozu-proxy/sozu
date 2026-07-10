//! I/O shell wiring the sans-io [`SniPrereadCore`] into the TCP session
//! lifecycle (issue #1279).
//!
//! [`SniPreread`] is the socket-owning half of the split described in the
//! module doc one level up: it accumulates bytes from the raw frontend
//! socket into the session's own [`Checkout`] (NEVER `consume()`d while
//! undecided -- byte-for-byte replay to the backend is the whole point),
//! re-feeds the full accumulated window to [`SniPrereadCore::handle_input`]
//! on every read, and -- once routed -- stays parked in this state through
//! backend connect. This mirrors `RelayProxyProtocol`, not `Pipe`:
//! `Pipe::set_back_socket` does not re-arm inherited buffer writes, so a
//! premature switch to `Pipe` before the backend socket exists would strand
//! the connect handshake.
//!
//! This module intentionally emits its OWN `tcp.sni_preread.*` decision
//! metrics (routed / rejected.<reason>) and log lines, but does **not** own
//! the `tcp.sni_preread.active` gauge lifecycle or the eventual state
//! transition out of `SniPreread` -- both live in `lib/src/tcp.rs`
//! (`TcpSession::close` and `TcpSession::upgrade_sni_preread`
//! respectively), which alone has the `TcpProxy`/`TcpListener` context
//! (per-cluster `proxy_protocol`, buffers, listener) needed to pick the
//! right next state and guarantee the gauge is adjusted exactly once per
//! session, on exactly one of its three exits (reject / upgrade / teardown).

use std::{net::SocketAddr, time::Instant};

use mio::{Token, net::TcpStream};
use rusty_ulid::Ulid;

use super::{Input, Output, PrereadConfig, SniPrereadCore};
use crate::{
    Readiness, SessionMetrics, SessionResult,
    metrics::names,
    pool::Checkout,
    socket::{SocketHandler, SocketResult},
    sozu_command::{ready::Ready, state::ClusterId},
};

/// Per-session prefix for log lines emitted with a [`SniPreread`] in scope.
/// Renders the canonical `\tTCP-SNI\tSession(...)\t >>>` envelope, reusing
/// the `TCP-SNI` tag already established by the core's own `log_context!`
/// (`tcp_preread/mod.rs`) so operators can grep core decisions and shell
/// orchestration together.
macro_rules! log_context {
    ($self:expr) => {{
        let (open, reset, grey, gray, white) = sozu_command::logging::ansi_palette();
        format!(
            "{open}TCP-SNI{reset}\t{grey}Session{reset}({gray}frontend{reset}={white}{frontend}{reset})\t >>>",
            open = open,
            reset = reset,
            grey = grey,
            gray = gray,
            white = white,
            frontend = $self.frontend_token.0,
        )
    }};
}

/// Captured exactly once, when [`Output::Routed`] first fires. The core's
/// own `decided` latch guarantees the SAME terminal verdict replays on every
/// subsequent `handle_input` call, so [`SniPreread::outcome`] never changes
/// once `Some`.
#[derive(Debug, Clone)]
pub struct RoutedOutcome {
    pub cluster: ClusterId,
    pub content_offset: usize,
    pub proxy_source: Option<SocketAddr>,
    pub sni: String,
    pub alpn: Vec<Vec<u8>>,
}

/// TCP session state that owns the frontend socket and the accumulating
/// [`Checkout`] while [`SniPrereadCore`] decides a route. See the module doc
/// for the full lifecycle and the exit-accounting contract for
/// `tcp.sni_preread.active`.
pub struct SniPreread<Front: SocketHandler> {
    pub frontend: Front,
    pub frontend_token: Token,
    pub frontend_readiness: Readiness,
    pub backend_readiness: Readiness,
    pub backend: Option<TcpStream>,
    pub backend_token: Option<Token>,
    pub request_id: Ulid,
    /// The session's own frontend accumulator, growing from wire offset 0.
    /// NEVER `consume()`d while undecided.
    pub frontend_buffer: Checkout,
    /// `min(listener.config.sni_preread_max_bytes, frontend_buffer.capacity())`,
    /// captured once at construction -- the buffer's capacity is fixed for
    /// its lifetime, so this never needs re-deriving.
    effective_max_bytes: usize,
    core: SniPrereadCore,
    outcome: Option<RoutedOutcome>,
    started_at: Instant,
}

impl<Front: SocketHandler> SniPreread<Front> {
    /// Instantiate a new SniPreread SessionState with:
    /// - frontend_interest: READABLE | HUP | ERROR
    /// - backend_interest: HUP | ERROR (WRITABLE armed once routed)
    pub fn new(
        frontend: Front,
        frontend_token: Token,
        request_id: Ulid,
        frontend_buffer: Checkout,
        effective_max_bytes: usize,
    ) -> Self {
        SniPreread {
            frontend,
            frontend_token,
            frontend_readiness: Readiness {
                interest: Ready::READABLE | Ready::HUP | Ready::ERROR,
                event: Ready::EMPTY,
            },
            backend_readiness: Readiness {
                interest: Ready::HUP | Ready::ERROR,
                event: Ready::EMPTY,
            },
            backend: None,
            backend_token: None,
            request_id,
            frontend_buffer,
            effective_max_bytes,
            core: SniPrereadCore::new(),
            outcome: None,
            started_at: Instant::now(),
        }
    }

    pub fn effective_max_bytes(&self) -> usize {
        self.effective_max_bytes
    }

    pub fn outcome(&self) -> Option<&RoutedOutcome> {
        self.outcome.as_ref()
    }

    pub fn is_routed(&self) -> bool {
        self.outcome.is_some()
    }

    pub fn started_at(&self) -> Instant {
        self.started_at
    }

    /// Bytes already accumulated from the frontend. Distinguishes a genuine
    /// mid-preread abort (bytes were seen) from a bare TCP health-check
    /// connect/FIN with no bytes at all (cf. `expect.rs`'s identical guard).
    pub fn has_received_bytes(&self) -> bool {
        self.frontend_buffer.available_data() > 0
    }

    pub fn front_socket(&self) -> &TcpStream {
        self.frontend.socket_ref()
    }

    pub fn back_socket_mut(&mut self) -> Option<&mut TcpStream> {
        self.backend.as_mut()
    }

    pub fn set_back_socket(&mut self, socket: TcpStream) {
        self.backend = Some(socket);
    }

    pub fn set_back_token(&mut self, token: Token) {
        self.backend_token = Some(token);
    }

    /// Read available bytes and feed them to the core. Emits the
    /// `tcp.sni_preread.routed` / `tcp.sni_preread.rejected.<reason>`
    /// metrics and log lines itself; the caller (`TcpSession::readable` in
    /// `lib/src/tcp.rs`) only needs to react to the returned
    /// [`SessionResult`] and, on a fresh route, sync `TcpSession::cluster_id`
    /// from [`Self::outcome`].
    pub fn readable(
        &mut self,
        metrics: &mut SessionMetrics,
        cfg: &PrereadConfig<'_>,
    ) -> SessionResult {
        if self.outcome.is_some() {
            // Already decided; nothing left to read for. The frontend
            // interest should already be quiesced by the caller once
            // routed, but stay defensive against a stray extra event.
            return SessionResult::Continue;
        }

        // Enforce `effective_max_bytes` as a HARD read bound, not merely an
        // advisory the core applies on its NeedMore path. The frontend
        // `Checkout` is sized to the pool buffer (>= 16 KiB), far larger than
        // a tight `sni_preread_max_bytes`; draining all of it would read an
        // oversized-but-COMPLETE ClientHello in full, which the core then
        // routes (it caps only would-be-NeedMore windows -- see
        // `mod.rs::need_more_or_too_large`). Capping the read makes an
        // over-cap hello reach the core INCOMPLETE with `buf.len() ==
        // max_bytes`, the exact `TooLarge` reject condition
        // (sozu-proxy/sozu#1279 Defect D). A hello that fits within the cap
        // still routes, and any coalesced bytes past the cap stay in the
        // kernel socket buffer for the `Pipe` to replay byte-for-byte.
        let cap_remaining = self
            .effective_max_bytes
            .saturating_sub(self.frontend_buffer.available_data());
        let space_before = self.frontend_buffer.available_space();
        let read_len = space_before.min(cap_remaining);
        let (sz, socket_result) = self
            .frontend
            .socket_read(&mut self.frontend_buffer.space()[..read_len]);
        // The socket can only write into the free space it was handed.
        debug_assert!(
            sz <= read_len,
            "socket_read cannot return more bytes than the capped space it was handed"
        );

        if sz > 0 {
            let data_before = self.frontend_buffer.available_data();
            self.frontend_buffer.fill(sz);
            debug_assert_eq!(
                self.frontend_buffer.available_data(),
                data_before + sz,
                "fill must expose exactly the bytes just read"
            );

            count!(names::backend::BYTES_IN, sz as i64);
            metrics.bin += sz;

            if socket_result == SocketResult::Error {
                return self.front_gone(cfg);
            }
            if socket_result == SocketResult::WouldBlock {
                self.frontend_readiness.event.remove(Ready::READABLE);
            }

            let output = self.core.handle_input(
                cfg,
                Input::Bytes {
                    buf: self.frontend_buffer.data(),
                    now: Instant::now(),
                },
            );
            return self.handle_output(output);
        }

        match socket_result {
            SocketResult::Error | SocketResult::Closed => self.front_gone(cfg),
            SocketResult::WouldBlock => {
                self.frontend_readiness.event.remove(Ready::READABLE);
                SessionResult::Continue
            }
            SocketResult::Continue => SessionResult::Continue,
        }
    }

    /// Feed the preread deadline firing. Always terminal: a fired deadline
    /// resolves to `Reject(Fragmented)` (or replays an already-latched
    /// verdict in the vanishingly unlikely race where routing and the
    /// deadline land in the same tick -- the core's `decided` latch makes
    /// that safe either way).
    pub fn on_timeout(&mut self, cfg: &PrereadConfig<'_>) {
        let output = self.core.handle_input(
            cfg,
            Input::Timeout {
                now: Instant::now(),
            },
        );
        // The frontend timeout always closes the session regardless; this
        // call exists purely for its metric/log side effect.
        let _ = self.handle_output(output);
    }

    /// Feed a frontend close observed before a routing decision (e.g. HUP
    /// racing ahead of `readable`'s own 0-byte detection, dispatched from
    /// `TcpSession::front_hup`). Mirrors `readable`'s
    /// `SocketResult::Closed` handling: silent when no bytes were ever
    /// received (the bare TCP health-check pattern), metered otherwise.
    pub fn on_front_closed(&mut self, cfg: &PrereadConfig<'_>) {
        let _ = self.front_gone(cfg);
    }

    /// Shared "the frontend is gone" handling for both the read-path
    /// (`Ok(0)` / socket error) and the HUP-path (`on_front_closed`).
    fn front_gone(&mut self, cfg: &PrereadConfig<'_>) -> SessionResult {
        if self.has_received_bytes() {
            let output = self.core.handle_input(cfg, Input::FrontClosed);
            self.handle_output(output)
        } else {
            // Bare TCP health-check pattern (SYN/ACK/FIN, no bytes ever
            // sent): silent, no rejection metric -- cf. `expect.rs`'s
            // identical guard.
            trace!(
                "{} front socket closed with 0 bytes during SNI preread",
                log_context!(self)
            );
            self.frontend_readiness.reset();
            SessionResult::Close
        }
    }

    fn handle_output(&mut self, output: Output) -> SessionResult {
        match output {
            Output::NeedMore { .. } => SessionResult::Continue,
            Output::Routed {
                cluster,
                content_offset,
                proxy_source,
                sni,
                alpn,
            } => {
                debug_assert!(
                    self.outcome.is_none(),
                    "a route must be captured at most once per SniPreread lifetime"
                );
                info!(
                    "{} SNI preread routed to cluster {} (sni={:?}, alpn={:?})",
                    log_context!(self),
                    cluster,
                    sni,
                    alpn.iter()
                        .map(|p| String::from_utf8_lossy(p).into_owned())
                        .collect::<Vec<_>>()
                );
                incr!(names::tcp::sni_preread::ROUTED);
                // Arm backend-writable interest now so the FIRST backend
                // connect-writable event (whenever `connect_to_backend`
                // completes) immediately dispatches into `back_writable`
                // without waiting for a second, unrelated readiness event --
                // mirrors `RelayProxyProtocol::readable`'s identical arm
                // once its own header has been parsed.
                self.backend_readiness.interest.insert(Ready::WRITABLE);
                self.outcome = Some(RoutedOutcome {
                    cluster,
                    content_offset,
                    proxy_source,
                    sni,
                    alpn,
                });
                SessionResult::Continue
            }
            Output::Reject(reason) => {
                debug!("{} SNI preread rejected: {:?}", log_context!(self), reason);
                incr!(names::tcp::sni_preread::rejected_name(reason));
                self.frontend_readiness.reset();
                self.backend_readiness.reset();
                SessionResult::Close
            }
        }
    }

    /// The `back_writable` dispatch point: only ever reachable once routed
    /// -- the `ready_inner` connect-gate in `lib/src/tcp.rs` guarantees
    /// `connect_to_backend` (and therefore any backend-writable event)
    /// never runs before a route decision. The actual per-`ProxyProtocolConfig`
    /// dispatch lives in `TcpSession::upgrade_sni_preread` (it needs the
    /// `TcpProxy`/listener context this struct deliberately does not hold),
    /// so this just signals the transition.
    pub fn back_writable(&self) -> SessionResult {
        debug_assert!(
            self.outcome.is_some(),
            "back_writable on SniPreread must only fire after a route decision"
        );
        SessionResult::Upgrade
    }
}

#[cfg(test)]
mod tests {
    use std::time::Duration;

    use super::*;
    use crate::protocol::tcp_preread::{AlpnMatcher, RejectReason};
    use crate::router::pattern_trie::TrieNode;

    /// `names::tcp::sni_preread::rejected_name` must be a TOTAL function
    /// over every `RejectReason` variant, each mapping to a DISTINCT dotted
    /// name -- a new variant added to the core without a matching arm fails
    /// to compile (see the `match` in `metrics/names.rs`), and this test
    /// additionally guards against two variants silently sharing one name.
    #[test]
    fn every_reject_reason_has_a_distinct_metric_name() {
        let reasons = [
            RejectReason::NotTls,
            RejectReason::MalformedRecord,
            RejectReason::MalformedHandshake,
            RejectReason::Fragmented,
            RejectReason::TooLarge,
            RejectReason::NoSni,
            RejectReason::EchOuterAbsent,
            RejectReason::SniUnmatched,
            RejectReason::AlpnUnmatched,
            RejectReason::ProxyHeaderInvalid,
            RejectReason::FrontClosed,
        ];
        let mut names_seen = std::collections::HashSet::new();
        for reason in reasons {
            let name = names::tcp::sni_preread::rejected_name(reason);
            assert!(
                name.starts_with("tcp.sni_preread.rejected."),
                "unexpected metric name shape for {reason:?}: {name}"
            );
            assert!(
                names_seen.insert(name),
                "two RejectReason variants mapped to the same metric name: {name}"
            );
        }
    }

    fn cfg(routes: &TrieNode<Vec<(AlpnMatcher, ClusterId)>>) -> PrereadConfig<'_> {
        PrereadConfig {
            routes,
            inbound_proxy: false,
            max_bytes: 16 * 1024,
            timeout: Duration::from_secs(3),
            accept_wildcard: true,
        }
    }

    #[test]
    fn preread_config_helper_shape_is_sane() {
        // Not a behavioral test of the core (already covered exhaustively
        // in `tcp_preread/mod.rs`) -- just pins the `PrereadConfig` field
        // shape this shell constructs against, so a field rename there is
        // caught here too.
        let routes = TrieNode::root();
        let cfg = cfg(&routes);
        assert!(!cfg.inbound_proxy);
        assert_eq!(cfg.max_bytes, 16 * 1024);
        assert!(cfg.accept_wildcard);
    }

    /// Regression guard for the un-enforced preread cap
    /// (sozu-proxy/sozu#1279 Defect D): `readable` must never pull more than
    /// `effective_max_bytes` into the accumulator in a single read, even
    /// though the backing buffer is far larger and the socket has far more
    /// queued. Without the cap the shell drains the whole buffer, so an
    /// oversized-but-COMPLETE ClientHello is read in full and ROUTED (the core
    /// caps only would-be-NeedMore windows) instead of rejected `TooLarge` and
    /// the connection closed.
    #[test]
    fn readable_caps_the_accumulator_at_effective_max_bytes() {
        use std::io::Write as _;
        use std::net::{TcpListener as StdTcpListener, TcpStream as StdTcpStream};

        use mio::net::TcpStream as MioTcpStream;

        use crate::pool::Pool;

        let listener = StdTcpListener::bind("127.0.0.1:0").expect("bind test listener");
        let addr = listener.local_addr().expect("listener local addr");
        let mut client = StdTcpStream::connect(addr).expect("connect test client");
        let (server, _) = listener.accept().expect("accept test server");
        server.set_nonblocking(true).expect("server nonblocking");

        // Flood far more than the tight cap, into a buffer far larger than it.
        let flood = vec![0x16u8; 4096];
        client.write_all(&flood).expect("write flood");
        client.flush().ok();

        let mut pool = Pool::with_capacity(1, 1, 16 * 1024);
        let frontend_buffer = pool.checkout().expect("frontend buffer");
        assert!(
            frontend_buffer.available_space() > 4096,
            "buffer must dwarf the cap for this test to be meaningful"
        );

        let effective_max_bytes = 16usize;
        let mut preread = SniPreread::new(
            MioTcpStream::from_std(server),
            Token(0),
            Ulid::generate(),
            frontend_buffer,
            effective_max_bytes,
        );

        // Keep the core cap consistent with the shell cap, exactly as
        // `TcpListener::preread_config` does (both = effective_max_bytes).
        let routes = TrieNode::root();
        let cfg = PrereadConfig {
            routes: &routes,
            inbound_proxy: false,
            max_bytes: effective_max_bytes,
            timeout: Duration::from_secs(3),
            accept_wildcard: true,
        };
        let mut metrics = SessionMetrics::new(Some(Duration::ZERO));

        let result = preread.readable(&mut metrics, &cfg);
        assert!(
            preread.frontend_buffer.available_data() <= effective_max_bytes,
            "readable accumulated {} bytes, past the {}-byte cap",
            preread.frontend_buffer.available_data(),
            effective_max_bytes
        );
        // A window that reached the cap without a complete hello rejects and
        // closes -- it must never sit routed or spin needing more.
        assert_eq!(
            result,
            SessionResult::Close,
            "an over-cap preread window must reject-and-close"
        );

        drop(client);
    }
}
