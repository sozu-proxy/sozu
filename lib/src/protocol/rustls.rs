//! Rustls handshake driver.
//!
//! Owns the per-session `rustls::ServerConnection` during the TLS
//! handshake: pumps `read_tls`/`write_tls`, surfaces handshake completion
//! to the parent state, and emits handshake-completion metrics. Cipher /
//! ALPN / SNI binding decisions live in `lib/src/https.rs`; certificate
//! resolution and dynamic cert reload live in `lib/src/tls.rs`.

use std::{
    cell::RefCell,
    io::{ErrorKind, Read},
    net::SocketAddr,
    rc::Rc,
    time::Instant,
};

use mio::{Token, net::TcpStream};
use rustls::{Error as RustlsError, ServerConnection};
use rusty_ulid::Ulid;
use sozu_command::{
    config::MAX_LOOP_ITERATIONS,
    logging::{LogContext, ansi_palette},
};

use crate::metrics::names;
use crate::{
    Readiness, Ready, SessionMetrics, SessionResult, StateResult, protocol::SessionState,
    socket::ShortReadProbe, timer::TimeoutContainer,
};

/// This macro is defined uniquely in this module to help the tracking of tls
/// issues inside Sōzu. When the logger emits to a TTY the protocol label is
/// bold bright-white (uniform across every protocol), the `Session` keyword is
/// light grey, attribute keys are gray and values are bright white. ANSI codes
/// are skipped when output goes to a file or otherwise non-colored sink. The
/// `[ulid - - -]` context prefix comes first to keep column alignment with
/// `MUX-*` and `SOCKET` logs.
macro_rules! log_context {
    ($self:expr) => {{
        let (open, reset, grey, gray, white) = ansi_palette();
        format!(
            "{gray}{ctx}{reset}\t{open}RUSTLS{reset}\t{grey}Session{reset}({gray}sni_bytes{reset}={white}{sni_bytes:?}{reset}, {gray}alpn_bytes{reset}={white}{alpn_bytes:?}{reset}, {gray}version{reset}={white}{version:?}{reset}, {gray}source{reset}={white}{source:?}{reset}, {gray}frontend{reset}={white}{frontend}{reset}, {gray}readiness{reset}={white}{readiness}{reset})\t >>>",
            open = open,
            reset = reset,
            grey = grey,
            gray = gray,
            white = white,
            ctx = $self.log_context(),
            sni_bytes = $self.session.server_name().map(str::len),
            alpn_bytes = $self.session.alpn_protocol().map(|bytes| bytes.len()),
            version = $self.session.protocol_version(),
            source = $self
                .peer_address
                .map(|addr| addr.to_string())
                .unwrap_or_else(|| "<none>".to_string()),
            frontend = $self.frontend_token.0,
            readiness = $self.frontend_readiness,
        )
    }};
}

/// Why [`handshake_read`] ended the handshake, handed back rather than logged
/// so [`TlsHandshake::readable`] can render it with its session context.
#[derive(Debug)]
enum HandshakeReadFault {
    /// `read_tls` answered `Ok(0)`: the peer closed during the handshake.
    Closed,
    /// `read_tls` failed with something other than `WouldBlock`.
    ReadTls(std::io::Error),
    /// `process_new_packets` rejected what `read_tls` delivered.
    ProcessPackets(RustlsError),
}

/// Read half of [`TlsHandshake::readable`], generic over the transport so
/// tests can count every `recv(2)` it issues.
///
/// Calls `read_tls` (one `recv`) and `process_new_packets` while rustls
/// `wants_read()`, and stops on the first of:
///
/// - EAGAIN;
/// - a short read: a `recv` that answered fewer bytes than the 4096 rustls
///   offered (`rustls-0.23.45/src/msgs/deframer/buffers.rs:208,220`) emptied
///   the receive queue, exactly as in [`crate::socket::ShortReadProbe`]'s
///   other caller, so a second `recv` could only answer EAGAIN. What it
///   delivered is still processed;
/// - `wants_read()` turning false: rustls holds plaintext, or has a flight to
///   send first (`rustls-0.23.45/src/common_state.rs:674-684`).
///
/// A TLS 1.3 server `wants_read()` again as soon as its own flight is queued,
/// because it may already send application data; without the stop on a short
/// read the ClientHello and the client `Finished` were each followed by a
/// `recv` that answered EAGAIN.
///
/// Readiness contract, the one `rustls_socket_read` (`lib/src/socket.rs`)
/// follows:
///
/// - EAGAIN and a short read both drop READABLE from `event`. mio registers
///   every socket edge-triggered (`mio/src/sys/unix/selector/epoll.rs`,
///   `EPOLLET`), and "an event will be generated upon each receipt of a chunk
///   of data" (`man 7 epoll`), so the next segment of a ClientHello split
///   across several raises the next event and the next call reads it. The
///   handshake never waits for bytes that are already queued;
/// - a read that fills what rustls offered is not a short read: the loop
///   reads again;
/// - HUP needs no exception here, unlike the mux (`update_readiness_after_read`
///   in `lib/src/protocol/mux/mod.rs`): `TlsHandshake::ready` closes the
///   session on HUP before reading anything;
/// - TCP urgent data is the exception to "a short read empties the queue":
///   `recv` stops before an urgent mark with bytes queued behind it (see
///   `plain_socket_read` in `lib/src/socket.rs`), so a peer that sends OOB
///   data inside its handshake stalls until its next send, as it does after
///   the handshake since #1606. TLS never uses urgent data.
///
/// HAProxy drives the handshake the same way: `ssl_sock_handshake`
/// (`src/ssl_sock.c:6462`) returns on `SSL_ERROR_WANT_READ` and subscribes
/// for the next receive event (`src/ssl_sock.c:6676-6682`), and its BIO reads
/// through `raw_sock_to_buf`, which stops on `ret < try` (`src/raw_sock.c:315-317`).
/// tokio-rustls loops `read_tls` until `Pending`
/// (`tokio-rustls-0.26.4/src/common/mod.rs:161-170`), and that `Pending` costs
/// no `recv` after a short read because tokio clears the readiness when
/// `0 < n < len` (`tokio-1.53.1/src/io/poll_evented.rs:211-213`).
fn handshake_read<R: Read>(
    session: &mut ServerConnection,
    stream: &mut R,
    event: &mut Ready,
) -> Result<(), HandshakeReadFault> {
    let mut can_read = true;
    while can_read && session.wants_read() {
        let mut probe = ShortReadProbe::new(&mut *stream);
        match session.read_tls(&mut probe) {
            Ok(0) => return Err(HandshakeReadFault::Closed),
            Ok(_) => {
                if probe.was_short() {
                    event.remove(Ready::READABLE);
                    can_read = false;
                }
            }
            Err(e) if e.kind() == ErrorKind::WouldBlock => {
                event.remove(Ready::READABLE);
                can_read = false;
            }
            Err(e) => return Err(HandshakeReadFault::ReadTls(e)),
        }
        session
            .process_new_packets()
            .map_err(HandshakeReadFault::ProcessPackets)?;
    }
    // Every exit that stops reading while rustls still wants to proved the
    // queue empty, and dropped READABLE with it.
    debug_assert!(
        can_read || !event.is_readable(),
        "the handshake stopped reading on an empty queue but kept READABLE"
    );
    Ok(())
}

pub enum TlsState {
    Initial,
    Handshake,
    Established,
    Error,
}

pub struct TlsHandshake {
    pub container_frontend_timeout: TimeoutContainer,
    pub frontend_readiness: Readiness,
    frontend_token: Token,
    pub peer_address: Option<SocketAddr>,
    pub request_id: Ulid,
    pub session: ServerConnection,
    pub stream: TcpStream,
    /// Wall-clock anchor for the `tls.handshake_ms` histogram. Captured the
    /// first time the handshake state actually does I/O (not at construction,
    /// because the session may sit in the accept queue or in expect-proxy for
    /// an unbounded amount of time before the TLS bytes start flowing).
    handshake_started_at: Option<Instant>,
}

impl TlsHandshake {
    /// Instantiate a new TlsHandshake SessionState with:
    ///
    /// - frontend_interest: READABLE | HUP | ERROR
    /// - frontend_event: EMPTY
    ///
    /// Remember to set the events from the previous State!
    pub fn new(
        container_frontend_timeout: TimeoutContainer,
        session: ServerConnection,
        stream: TcpStream,
        frontend_token: Token,
        request_id: Ulid,
        peer_address: Option<SocketAddr>,
    ) -> TlsHandshake {
        TlsHandshake {
            container_frontend_timeout,
            frontend_readiness: Readiness {
                interest: Ready::READABLE | Ready::HUP | Ready::ERROR,
                event: Ready::EMPTY,
            },
            frontend_token,
            peer_address,
            request_id,
            session,
            stream,
            handshake_started_at: None,
        }
    }

    /// Returns the elapsed handshake duration in milliseconds and clears the
    /// captured start instant so the histogram is only recorded once. Returns
    /// `None` when no I/O happened (e.g. the connection closed mid-handshake
    /// before any bytes were exchanged); callers should not emit
    /// `tls.handshake_ms` in that case.
    fn record_handshake_duration_ms(&mut self) -> Option<u128> {
        let was_anchored = self.handshake_started_at.is_some();
        let elapsed = self
            .handshake_started_at
            .take()
            .map(|t| t.elapsed().as_millis());
        // `take()` is idempotent-disarming: the anchor is always cleared so the
        // histogram is recorded at most once, and a duration is returned iff an
        // anchor existed.
        debug_assert!(
            self.handshake_started_at.is_none(),
            "handshake anchor must be cleared after recording the duration"
        );
        debug_assert_eq!(
            elapsed.is_some(),
            was_anchored,
            "a duration is returned iff the handshake had been anchored"
        );
        elapsed
    }

    pub fn readable(&mut self) -> SessionResult {
        // Anchor the handshake duration the first time we observe TLS bytes
        // moving in either direction. Using `get_or_insert_with` keeps the
        // anchor sticky across `WouldBlock` retries and across the
        // readable/writable boundary.
        self.handshake_started_at.get_or_insert_with(Instant::now);
        // The anchor is sticky once set: this method must never run unanchored.
        debug_assert!(
            self.handshake_started_at.is_some(),
            "handshake anchor must be set before driving TLS I/O"
        );

        // rustls handshake completion is monotonic (`true → false`, never
        // back). Snapshot it so the exit assertions can prove we never resurrect
        // a finished handshake.
        let was_handshaking = self.session.is_handshaking();

        match handshake_read(
            &mut self.session,
            &mut self.stream,
            &mut self.frontend_readiness.event,
        ) {
            Ok(()) => {}
            Err(HandshakeReadFault::Closed) => {
                error!("{} Connection closed during handshake", log_context!(self));
                return SessionResult::Close;
            }
            Err(HandshakeReadFault::ReadTls(e)) => {
                error!(
                    "{} Could not perform handshake: {:?}",
                    log_context!(self),
                    e
                );
                return SessionResult::Close;
            }
            Err(HandshakeReadFault::ProcessPackets(e)) => {
                self.log_handshake_error(&e);
                return SessionResult::Close;
            }
        }

        // Handshake completion is monotonic: a handshake that had already
        // finished at entry cannot become unfinished by pumping `read_tls`.
        debug_assert!(
            was_handshaking || !self.session.is_handshaking(),
            "rustls handshake must not regress from finished back to handshaking"
        );

        // Readiness must mirror rustls's own wants: we only drop READABLE
        // interest when the session no longer wants to read.
        if !self.session.wants_read() {
            self.frontend_readiness.interest.remove(Ready::READABLE);
        }
        debug_assert!(
            self.session.wants_read() || !self.frontend_readiness.interest.is_readable(),
            "READABLE interest must be cleared once rustls stops wanting reads"
        );

        if self.session.wants_write() {
            self.frontend_readiness.interest.insert(Ready::WRITABLE);
        }

        if self.session.is_handshaking() {
            SessionResult::Continue
        } else {
            // handshake might be finished, but we still have something to send
            if self.session.wants_write() {
                SessionResult::Continue
            } else {
                // Upgrade is only signalled once the handshake is complete and
                // there is nothing left to flush to the peer.
                debug_assert!(
                    !self.session.is_handshaking() && !self.session.wants_write(),
                    "Upgrade requires a completed handshake with no pending output"
                );
                // No `event.insert(READABLE)` here: whether the upgraded
                // session must read at once is decided by
                // `upgraded_frontend_events` (`lib/src/https.rs`) from what is
                // left in `event` — READABLE survives only when no read proved
                // the socket empty since the last edge — and from the
                // plaintext rustls already holds. Forcing it here made every
                // upgraded session issue one `recv` that answered EAGAIN.
                self.frontend_readiness.interest.insert(Ready::READABLE);
                self.frontend_readiness.interest.insert(Ready::WRITABLE);
                if let Some(elapsed_ms) = self.record_handshake_duration_ms() {
                    time!(names::tls::HANDSHAKE_MS, elapsed_ms);
                }
                SessionResult::Upgrade
            }
        }
    }

    pub fn writable(&mut self) -> SessionResult {
        // Same anchor logic as `readable()` — see the comment there.
        self.handshake_started_at.get_or_insert_with(Instant::now);
        debug_assert!(
            self.handshake_started_at.is_some(),
            "handshake anchor must be set before driving TLS I/O"
        );

        // Snapshot handshake completion for the monotonicity post-condition.
        let was_handshaking = self.session.is_handshaking();

        let mut can_write = true;

        loop {
            let mut can_work = false;

            if self.session.wants_write() && can_write {
                can_work = true;

                match self.session.write_tls(&mut self.stream) {
                    Ok(_) => {}
                    Err(e) => match e.kind() {
                        ErrorKind::WouldBlock => {
                            self.frontend_readiness.event.remove(Ready::WRITABLE);
                            can_write = false
                        }
                        _ => {
                            error!(
                                "{} Could not perform handshake: {:?}",
                                log_context!(self),
                                e
                            );
                            return SessionResult::Close;
                        }
                    },
                }

                if let Err(e) = self.session.process_new_packets() {
                    self.log_handshake_error(&e);
                    return SessionResult::Close;
                }
            }

            if !can_work {
                break;
            }
        }

        // Handshake completion is monotonic: pumping `write_tls` can finish a
        // handshake but never un-finish one.
        debug_assert!(
            was_handshaking || !self.session.is_handshaking(),
            "rustls handshake must not regress from finished back to handshaking"
        );

        // Readiness mirrors rustls's wants: WRITABLE interest is only dropped
        // once the session no longer wants to write.
        if !self.session.wants_write() {
            self.frontend_readiness.interest.remove(Ready::WRITABLE);
        }
        debug_assert!(
            self.session.wants_write() || !self.frontend_readiness.interest.is_writable(),
            "WRITABLE interest must be cleared once rustls stops wanting writes"
        );

        if self.session.wants_read() {
            self.frontend_readiness.interest.insert(Ready::READABLE);
        }

        if self.session.is_handshaking() {
            SessionResult::Continue
        } else if self.session.wants_read() {
            // Upgrade after a completed handshake; the session still wants to
            // read application data, which the upgraded state will drive.
            debug_assert!(
                !self.session.is_handshaking(),
                "Upgrade requires a completed handshake"
            );
            self.frontend_readiness.interest.insert(Ready::READABLE);
            if let Some(elapsed_ms) = self.record_handshake_duration_ms() {
                time!(names::tls::HANDSHAKE_MS, elapsed_ms);
            }
            SessionResult::Upgrade
        } else {
            debug_assert!(
                !self.session.is_handshaking(),
                "Upgrade requires a completed handshake"
            );
            self.frontend_readiness.interest.insert(Ready::WRITABLE);
            self.frontend_readiness.interest.insert(Ready::READABLE);
            if let Some(elapsed_ms) = self.record_handshake_duration_ms() {
                time!(names::tls::HANDSHAKE_MS, elapsed_ms);
            }
            SessionResult::Upgrade
        }
    }

    pub fn log_context(&self) -> LogContext<'_> {
        LogContext {
            session_id: self.request_id,
            request_id: None,
            cluster_id: None,
            backend_id: None,
        }
    }

    pub fn front_socket(&self) -> &TcpStream {
        &self.stream
    }

    /// Tiered logging for TLS handshake errors surfaced by `process_new_packets`.
    ///
    /// - `AlertReceived(_)`: remote peer rejected our cert/config (e.g. old
    ///   CA bundle, scanner, cert-pinning client). Not actionable per-connection
    ///   on a public endpoint, so log at `debug!`.
    /// - Peer protocol violations (`PeerIncompatible`, `PeerMisbehaved`,
    ///   `InvalidMessage`, inappropriate message / handshake message,
    ///   oversized record, ALPN mismatch, bad client cert, `DecryptError`,
    ///   `NoCertificatesPresented`): occasionally useful to spot buggy
    ///   clients or stale roots, so log at `warn!`.
    /// - Everything else (local/config/provider failures like `EncryptError`,
    ///   `General`, `Other`, CRL issues, missing entropy): genuine server-side
    ///   problems, stay at `error!`.
    ///
    /// Each tier additionally bumps `tls.handshake.failed.<reason>` so dashboards
    /// can split spikes by category without having to grep logs.
    fn log_handshake_error(&self, err: &RustlsError) {
        let reason = handshake_failure_reason(err);
        // Every reason must stay inside the bounded `tls.handshake.failed.*`
        // namespace so statsd cardinality is predictable — unknown variants
        // collapse to `.other`, never an unnamespaced key.
        debug_assert!(
            reason.starts_with("tls.handshake.failed."),
            "handshake failure metric {reason} escaped the tls.handshake.failed. namespace"
        );
        match err {
            RustlsError::AlertReceived(_) => debug!(
                "{} Could not perform handshake: {:?}",
                log_context!(self),
                err
            ),
            RustlsError::PeerIncompatible(_)
            | RustlsError::PeerMisbehaved(_)
            | RustlsError::InvalidMessage(_)
            | RustlsError::InappropriateMessage { .. }
            | RustlsError::InappropriateHandshakeMessage { .. }
            | RustlsError::PeerSentOversizedRecord
            | RustlsError::NoApplicationProtocol
            | RustlsError::InvalidCertificate(_)
            | RustlsError::DecryptError
            | RustlsError::NoCertificatesPresented => warn!(
                "{} Could not perform handshake: {:?}",
                log_context!(self),
                err
            ),
            _ => error!(
                "{} Could not perform handshake: {:?}",
                log_context!(self),
                err
            ),
        }
        count!(reason, 1);
    }
}

/// Compile-time literal `tls.handshake.failed.<reason>` keys for every variant
/// the proxy can observe. Free function (rather than a method) so unit tests
/// can drive it without constructing a real `ServerConnection`. The set of
/// suffixes is bounded — anything outside the explicit `match` arms collapses
/// to `tls.handshake.failed.other` so statsd cardinality stays predictable.
fn handshake_failure_reason(err: &RustlsError) -> &'static str {
    match err {
        RustlsError::AlertReceived(_) => "tls.handshake.failed.alert_received",
        RustlsError::PeerIncompatible(_) => "tls.handshake.failed.peer_incompatible",
        RustlsError::PeerMisbehaved(_) => "tls.handshake.failed.peer_misbehaved",
        RustlsError::InvalidMessage(_) => "tls.handshake.failed.invalid_message",
        RustlsError::InappropriateMessage { .. } => "tls.handshake.failed.inappropriate_message",
        RustlsError::InappropriateHandshakeMessage { .. } => {
            "tls.handshake.failed.inappropriate_handshake_message"
        }
        RustlsError::PeerSentOversizedRecord => "tls.handshake.failed.oversized_record",
        RustlsError::NoApplicationProtocol => "tls.handshake.failed.no_alpn",
        RustlsError::InvalidCertificate(_) => "tls.handshake.failed.invalid_certificate",
        RustlsError::DecryptError => "tls.handshake.failed.decrypt_error",
        RustlsError::NoCertificatesPresented => "tls.handshake.failed.no_certificates_present",
        _ => "tls.handshake.failed.other",
    }
}

impl SessionState for TlsHandshake {
    fn ready(
        &mut self,
        _session: Rc<RefCell<dyn crate::ProxySession>>,
        _proxy: Rc<RefCell<dyn crate::L7Proxy>>,
        _metrics: &mut SessionMetrics,
    ) -> SessionResult {
        let mut counter = 0;

        if self.frontend_readiness.event.is_hup() {
            return SessionResult::Close;
        }

        while counter < MAX_LOOP_ITERATIONS {
            let frontend_interest = self.frontend_readiness.filter_interest();

            trace!("{} Interest({:?})", log_context!(self), frontend_interest);
            if frontend_interest.is_empty() {
                break;
            }

            if frontend_interest.is_readable() {
                let protocol_result = self.readable();
                if protocol_result != SessionResult::Continue {
                    return protocol_result;
                }
            }

            if frontend_interest.is_writable() {
                let protocol_result = self.writable();
                if protocol_result != SessionResult::Continue {
                    return protocol_result;
                }
            }

            if frontend_interest.is_error() {
                error!("{} Front socket error, disconnecting", log_context!(self));
                self.frontend_readiness.interest = Ready::EMPTY;
                return SessionResult::Close;
            }

            counter += 1;
        }

        if counter >= MAX_LOOP_ITERATIONS {
            error!(
                "{}\tHandling session went through {} iterations, there's a probable infinite loop bug, closing the connection",
                log_context!(self),
                MAX_LOOP_ITERATIONS
            );

            incr!(names::http::INFINITE_LOOP_ERROR);
            self.print_state("HTTPS");

            return SessionResult::Close;
        }

        SessionResult::Continue
    }

    fn update_readiness(&mut self, token: Token, events: Ready) {
        if self.frontend_token == token {
            self.frontend_readiness.event |= events;
        }
    }

    fn timeout(&mut self, token: Token, _metrics: &mut SessionMetrics) -> StateResult {
        // relevant timeout is still stored in the Session as front_timeout.
        if self.frontend_token == token {
            self.container_frontend_timeout.triggered();
            return StateResult::CloseSession;
        }

        error!(
            "{}, Expect state: got timeout for an invalid token: {:?}",
            log_context!(self),
            token
        );
        StateResult::CloseSession
    }

    fn cancel_timeouts(&mut self) {
        self.container_frontend_timeout.cancel();
    }

    fn print_state(&self, context: &str) {
        error!(
            "{} Session(Handshake)\n\tFrontend:\n\t\ttoken: {:?}\treadiness: {:?}",
            context, self.frontend_token, self.frontend_readiness
        );
    }
}

// -----------------------------------------------------------------------------
// Unit tests

#[cfg(test)]
mod tests {
    use std::collections::HashSet;

    use rustls::{
        AlertDescription, CertificateError, ContentType, Error as RustlsError, HandshakeType,
        InvalidMessage, PeerIncompatible, PeerMisbehaved,
    };

    use mio::{Token, net::TcpStream};
    use rusty_ulid::Ulid;

    use super::{TlsHandshake, handshake_failure_reason, handshake_read};
    use crate::{Ready, SessionResult, protocol::SessionState, timer::TimeoutContainer};
    use std::io::ErrorKind;

    /// Every rustls error variant the proxy can observe must map to a distinct,
    /// compile-time literal `tls.handshake.failed.<reason>` key. Unknown
    /// variants (future rustls additions, `General`, `Other`, CRL errors, etc.)
    /// collapse to `tls.handshake.failed.other` so statsd cardinality stays
    /// bounded. This test also guards against accidental duplicate keys.
    #[test]
    fn handshake_failure_reason_maps_every_variant_to_unique_namespaced_key() {
        let cases: &[(RustlsError, &str)] = &[
            (
                RustlsError::AlertReceived(AlertDescription::HandshakeFailure),
                "tls.handshake.failed.alert_received",
            ),
            (
                RustlsError::PeerIncompatible(PeerIncompatible::NoCipherSuitesInCommon),
                "tls.handshake.failed.peer_incompatible",
            ),
            (
                RustlsError::PeerMisbehaved(PeerMisbehaved::IllegalMiddleboxChangeCipherSpec),
                "tls.handshake.failed.peer_misbehaved",
            ),
            (
                RustlsError::InvalidMessage(InvalidMessage::InvalidContentType),
                "tls.handshake.failed.invalid_message",
            ),
            (
                RustlsError::InappropriateMessage {
                    expect_types: vec![ContentType::Handshake],
                    got_type: ContentType::ApplicationData,
                },
                "tls.handshake.failed.inappropriate_message",
            ),
            (
                RustlsError::InappropriateHandshakeMessage {
                    expect_types: vec![HandshakeType::ClientHello],
                    got_type: HandshakeType::Finished,
                },
                "tls.handshake.failed.inappropriate_handshake_message",
            ),
            (
                RustlsError::PeerSentOversizedRecord,
                "tls.handshake.failed.oversized_record",
            ),
            (
                RustlsError::NoApplicationProtocol,
                "tls.handshake.failed.no_alpn",
            ),
            (
                RustlsError::InvalidCertificate(CertificateError::Expired),
                "tls.handshake.failed.invalid_certificate",
            ),
            (
                RustlsError::DecryptError,
                "tls.handshake.failed.decrypt_error",
            ),
            (
                RustlsError::NoCertificatesPresented,
                "tls.handshake.failed.no_certificates_present",
            ),
            // `Other` bucket — any variant not in the explicit list collapses here.
            (
                RustlsError::General("test".to_owned()),
                "tls.handshake.failed.other",
            ),
            (RustlsError::EncryptError, "tls.handshake.failed.other"),
            (
                RustlsError::FailedToGetCurrentTime,
                "tls.handshake.failed.other",
            ),
            (
                RustlsError::HandshakeNotComplete,
                "tls.handshake.failed.other",
            ),
        ];

        let mut seen = HashSet::new();
        for (err, expected) in cases {
            let got = handshake_failure_reason(err);
            assert_eq!(got, *expected, "variant {err:?} → {got}, want {expected}");
            assert!(
                got.starts_with("tls.handshake.failed."),
                "reason {got} missing tls.handshake.failed. namespace"
            );
            seen.insert(got);
        }

        // 11 explicit buckets + 1 shared `other` bucket = 12 distinct keys.
        assert_eq!(seen.len(), 12, "unexpected key set: {seen:?}");
    }

    /// Everything `conn` has queued for the wire.
    fn flight<D>(conn: &mut rustls::ConnectionCommon<D>) -> Vec<u8> {
        let mut wire = Vec::new();
        while conn.wants_write() {
            conn.write_tls(&mut wire)
                .expect("a TLS flight must serialize into memory");
        }
        wire
    }

    /// Feed a whole server flight to the client, in memory.
    fn deliver(client: &mut rustls::ClientConnection, wire: &[u8]) {
        let mut cursor = std::io::Cursor::new(wire);
        while (cursor.position() as usize) < wire.len() {
            client
                .read_tls(&mut cursor)
                .expect("the server flight must be readable from memory");
            client
                .process_new_packets()
                .expect("the server flight must process cleanly");
        }
    }

    /// The ClientHello and the client `Finished` each fit in one `recv`
    /// shorter than the 4096 bytes rustls offers: each costs that one `recv`
    /// and drops READABLE, with no second `recv` answering EAGAIN (#1609).
    #[test]
    fn a_short_handshake_read_stops_without_an_eagain() {
        use crate::socket::rustls_read_tests::{CountingTransport, fresh_pair};

        let (mut server, mut client) = fresh_pair(Vec::new());
        let hello = flight(&mut client);
        assert!(
            hello.len() < 4096,
            "premise: the ClientHello is a short read"
        );
        let mut transport = CountingTransport {
            wire: hello.into_iter().collect(),
            ..Default::default()
        };

        let mut event = Ready::READABLE;
        handshake_read(&mut server, &mut transport, &mut event)
            .expect("the ClientHello must process");
        assert_eq!((transport.reads, transport.eagains), (1, 0));
        assert!(
            !event.is_readable(),
            "a short read proved the queue empty: READABLE must go"
        );
        assert!(server.is_handshaking());

        deliver(&mut client, &flight(&mut server));
        transport.wire.extend(flight(&mut client));
        event = Ready::READABLE;
        handshake_read(&mut server, &mut transport, &mut event)
            .expect("the client Finished must process");
        assert_eq!(
            (transport.reads, transport.eagains),
            (2, 0),
            "the Finished costs one recv and no EAGAIN"
        );
        assert!(!event.is_readable());
        assert!(!server.is_handshaking(), "the handshake is complete");
    }

    /// A read that fills the 4096 bytes rustls offered is not a short read:
    /// the loop reads again. A ClientHello padded past 4096 bytes by a long
    /// ALPN list takes one full read and one short one, still no EAGAIN.
    #[test]
    fn a_full_handshake_read_reads_again() {
        use crate::socket::rustls_read_tests::{CountingTransport, fresh_pair};

        let alpn = (0..64u8)
            .map(|index| vec![b'a' + index % 26; 100])
            .collect();
        let (mut server, mut client) = fresh_pair(alpn);
        let hello = flight(&mut client);
        assert!(
            4096 < hello.len() && hello.len() < 8192,
            "premise: one full read and one short one, got {} bytes",
            hello.len()
        );
        let mut transport = CountingTransport {
            wire: hello.into_iter().collect(),
            ..Default::default()
        };
        let mut event = Ready::READABLE;
        handshake_read(&mut server, &mut transport, &mut event)
            .expect("the padded ClientHello must process");
        assert_eq!(
            (transport.reads, transport.eagains),
            (2, 0),
            "a full read is followed by one more recv, which is short"
        );
        assert!(!event.is_readable());
        assert!(server.wants_write(), "the whole ClientHello was processed");
    }

    /// Drive a handshake the way `TlsHandshake::ready` does, for one event.
    fn drive(handshake: &mut TlsHandshake) -> SessionResult {
        for _ in 0..16 {
            let interest = handshake.frontend_readiness.filter_interest();
            if interest.is_empty() {
                return SessionResult::Continue;
            }
            if interest.is_readable() {
                let result = handshake.readable();
                if result != SessionResult::Continue {
                    return result;
                }
            }
            if interest.is_writable() {
                let result = handshake.writable();
                if result != SessionResult::Continue {
                    return result;
                }
            }
        }
        panic!("the handshake did not settle within 16 passes");
    }

    /// A ClientHello split over three TCP segments completes on a real
    /// loopback socket registered edge-triggered with mio. After each short
    /// read the handshake drops READABLE, no event is pending, and the next
    /// segment raises the edge that resumes it: stopping on a short read never
    /// leaves the handshake waiting for bytes already queued. The upgrade that
    /// follows the client `Finished` leaves READABLE unset, because the last
    /// read proved the socket empty (#1609).
    #[test]
    fn a_client_hello_in_three_segments_completes_on_a_loopback_socket() {
        use std::io::{Read as _, Write as _};

        use crate::socket::rustls_read_tests::fresh_pair;

        let token = Token(7);
        let mut poll = mio::Poll::new().expect("a poll instance must open");
        let mut events = mio::Events::with_capacity(8);
        let listener = std::net::TcpListener::bind("127.0.0.1:0")
            .expect("test listener must bind to a loopback port");
        let mut peer = std::net::TcpStream::connect(
            listener
                .local_addr()
                .expect("test listener must report its local address"),
        )
        .expect("loopback connect must complete");
        peer.set_nodelay(true)
            .expect("the client must disable Nagle to send each segment at once");
        let (accepted, _) = listener.accept().expect("the connection must be accepted");
        accepted
            .set_nonblocking(true)
            .expect("mio requires a nonblocking stream");
        let mut stream = TcpStream::from_std(accepted);
        poll.registry()
            .register(&mut stream, token, mio::Interest::READABLE)
            .expect("the stream must register");

        let (server, mut client) = fresh_pair(Vec::new());
        let mut handshake = TlsHandshake::new(
            TimeoutContainer::new_empty(std::time::Duration::from_secs(10)),
            server,
            stream,
            token,
            Ulid::generate(),
            None,
        );
        let mut edge_within = |poll: &mut mio::Poll, timeout: std::time::Duration| {
            poll.poll(&mut events, Some(timeout))
                .expect("poll must succeed");
            events
                .iter()
                .any(|event| event.token() == token && event.is_readable())
        };
        let edge = std::time::Duration::from_secs(5);
        let quiet = std::time::Duration::from_millis(50);

        let hello = flight(&mut client);
        let third = hello.len() / 3;
        let segments = [
            &hello[..third],
            &hello[third..2 * third],
            &hello[2 * third..],
        ];
        for (index, segment) in segments.iter().enumerate() {
            peer.write_all(segment).expect("the client must send");
            assert!(
                edge_within(&mut poll, edge),
                "segment {index} must raise an edge"
            );
            handshake.update_readiness(token, Ready::READABLE);
            assert_eq!(drive(&mut handshake), SessionResult::Continue);
            assert!(
                !handshake.frontend_readiness.event.is_readable(),
                "segment {index}: the short read must drop READABLE"
            );
            if index + 1 < segments.len() {
                assert!(handshake.session.is_handshaking());
                assert!(
                    !edge_within(&mut poll, quiet),
                    "segment {index}: edge-triggered, nothing new, no event"
                );
                let mut probe = [0u8; 1];
                assert!(
                    matches!(
                        handshake.stream.read(&mut probe),
                        Err(ref e) if e.kind() == ErrorKind::WouldBlock
                    ),
                    "segment {index}: nothing is left queued"
                );
            }
        }
        assert!(
            handshake.session.wants_write(),
            "the whole ClientHello was read and processed"
        );

        // The server flight: write it as `ready` would once WRITABLE fires.
        handshake.update_readiness(token, Ready::WRITABLE);
        assert_eq!(drive(&mut handshake), SessionResult::Continue);
        assert!(!handshake.session.wants_write(), "the server flight is out");

        peer.set_read_timeout(Some(std::time::Duration::from_secs(5)))
            .expect("the client read timeout must set");
        while client.is_handshaking() {
            client
                .read_tls(&mut peer)
                .expect("the client must read the server flight");
            client
                .process_new_packets()
                .expect("the server flight must process cleanly");
        }
        peer.write_all(&flight(&mut client))
            .expect("the client must send its Finished");
        assert!(
            edge_within(&mut poll, edge),
            "the Finished must raise an edge"
        );
        handshake.update_readiness(token, Ready::READABLE);
        assert_eq!(drive(&mut handshake), SessionResult::Upgrade);
        assert!(!handshake.session.is_handshaking());
        assert!(
            !handshake.frontend_readiness.event.is_readable(),
            "the socket is empty: the upgrade must not be handed a READABLE"
        );
    }
}
