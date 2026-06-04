//! Rustls handshake driver.
//!
//! Owns the per-session `rustls::ServerConnection` during the TLS
//! handshake: pumps `read_tls`/`write_tls`, surfaces handshake completion
//! to the parent state, and emits handshake-completion metrics. Cipher /
//! ALPN / SNI binding decisions live in `lib/src/https.rs`; certificate
//! resolution and dynamic cert reload live in `lib/src/tls.rs`.

use std::{cell::RefCell, io::ErrorKind, net::SocketAddr, rc::Rc, time::Instant};

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
    timer::TimeoutContainer,
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
            "{gray}{ctx}{reset}\t{open}RUSTLS{reset}\t{grey}Session{reset}({gray}sni{reset}={white}{sni:?}{reset}, {gray}alpn{reset}={white}{alpn}{reset}, {gray}version{reset}={white}{version:?}{reset}, {gray}source{reset}={white}{source:?}{reset}, {gray}frontend{reset}={white}{frontend}{reset}, {gray}readiness{reset}={white}{readiness}{reset})\t >>>",
            open = open,
            reset = reset,
            grey = grey,
            gray = gray,
            white = white,
            ctx = $self.log_context(),
            sni = $self
                .session
                .server_name()
                .map(|addr| addr.to_string())
                .unwrap_or_else(|| "<none>".to_string()),
            alpn = $self
                .session
                .alpn_protocol()
                .map(|bytes| String::from_utf8_lossy(bytes).into_owned())
                .unwrap_or_else(|| "<none>".to_string()),
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

        let mut can_read = true;

        loop {
            let mut can_work = false;

            if self.session.wants_read() && can_read {
                can_work = true;

                match self.session.read_tls(&mut self.stream) {
                    Ok(0) => {
                        error!("{} Connection closed during handshake", log_context!(self));
                        return SessionResult::Close;
                    }
                    Ok(_) => {}
                    Err(e) => match e.kind() {
                        ErrorKind::WouldBlock => {
                            self.frontend_readiness.event.remove(Ready::READABLE);
                            can_read = false
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
                self.frontend_readiness.interest.insert(Ready::READABLE);
                self.frontend_readiness.event.insert(Ready::READABLE);
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

    use super::handshake_failure_reason;

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
}
