//! Cleartext HTTP/2 prior-knowledge detection (sozu-proxy/sozu#1911).
//!
//! An HTTP listener with `h2c_prior_knowledge = true` does not know, at
//! accept time, whether the client speaks HTTP/1.x or HTTP/2: over cleartext
//! there is no ALPN. RFC 9113 §3.3 lets a client that knows the server speaks
//! HTTP/2 open the connection with the 24-byte preface
//! `PRI * HTTP/2.0\r\n\r\nSM\r\n\r\n` directly. [`H2cPrefaceSniff`] is the
//! session stage that tells the two apart before any byte is consumed.
//!
//! The sniff reads with `MSG_PEEK` (`TcpStream::peek`), never with `recv`:
//! the bytes stay in the kernel receive queue, so the mux connection built
//! next reads them itself — `Connection::new_h2_server`
//! (`lib/src/protocol/mux/connection.rs`) parses and validates the preface
//! exactly as it does after a TLS ALPN `h2`, and `Connection::new_h1_server`
//! parses the HTTP/1.x request from its first byte. Nothing is copied, so no
//! buffer hand-off between the stages exists to get wrong.
//!
//! Decision rule ([`classify_preface`]): the peeked bytes are compared with
//! the preface prefix of the same length. A divergence at any byte means
//! HTTP/1.x; the whole preface means HTTP/2; a strict prefix means "wait for
//! more". Waiting is bounded by the session's request timeout, armed at
//! accept and carried into the mux unchanged, exactly as for the PROXY
//! protocol stage (`ExpectProxyProtocol`,
//! `lib/src/protocol/proxy_protocol/expect.rs`).
//!
//! Edge-triggered readiness: a peek drains nothing, but the kernel raises a
//! fresh EPOLLIN edge for every segment that arrives, so parking on a partial
//! prefix with READABLE cleared is safe. The reverse also holds: once the
//! sniff decides, the bytes it peeked raise no new edge, so the session must
//! hand READABLE to the mux frontend itself (`HttpSession::upgrade_sniff`,
//! `lib/src/http.rs`).

use std::{cell::RefCell, io::ErrorKind, net::SocketAddr, rc::Rc};

use mio::{Token, net::TcpStream};
use rusty_ulid::Ulid;
use sozu_command::{config::MAX_LOOP_ITERATIONS, logging::ansi_palette};

use crate::{
    L7Proxy, ProxySession, Readiness, SessionMetrics, StateResult,
    metrics::names,
    protocol::{SessionResult, SessionState, mux::parser::CLIENT_PREFACE_MAGIC},
    sozu_command::ready::Ready,
    timer::TimeoutContainer,
};

/// Length of the HTTP/2 client connection preface magic: the most bytes the
/// sniff ever needs to see.
pub const H2C_PREFACE_LEN: usize = CLIENT_PREFACE_MAGIC.len();

/// Per-session prefix for log lines emitted with an [`H2cPrefaceSniff`] in
/// scope. Renders the canonical `HTTP-H2C\tSession(...)\t >>>` envelope keyed
/// by the frontend token, so the lines correlate with the `HTTP` and `MUX-*`
/// lines of the same connection.
macro_rules! log_context {
    ($self:expr) => {{
        let (open, reset, grey, gray, white) = ansi_palette();
        format!(
            "{open}HTTP-H2C{reset}\t{grey}Session{reset}({gray}frontend{reset}={white}{frontend}{reset}, {gray}readiness{reset}={white}{readiness}{reset})\t >>>",
            open = open,
            reset = reset,
            grey = grey,
            gray = gray,
            white = white,
            frontend = $self.frontend_token.0,
            readiness = $self.frontend_readiness,
        )
    }};
}

/// What the first bytes of a cleartext connection say about its protocol.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum PrefaceVerdict {
    /// The full 24-byte HTTP/2 preface: serve the connection as HTTP/2.
    H2,
    /// A byte diverged from the preface: serve the connection as HTTP/1.x.
    Http1,
    /// Every byte so far matches the preface, but fewer than 24 arrived.
    NeedMore,
}

/// Classify the first bytes of a cleartext connection.
///
/// Only the first [`H2C_PREFACE_LEN`] bytes are considered; anything after
/// the preface (the client SETTINGS frame) is the H2 connection's business.
/// An empty slice is [`PrefaceVerdict::NeedMore`]: end of stream is the
/// caller's to detect, from the read that returned nothing.
pub fn classify_preface(peeked: &[u8]) -> PrefaceVerdict {
    let seen = peeked.len().min(H2C_PREFACE_LEN);
    let verdict = if peeked[..seen] != CLIENT_PREFACE_MAGIC[..seen] {
        PrefaceVerdict::Http1
    } else if seen == H2C_PREFACE_LEN {
        PrefaceVerdict::H2
    } else {
        PrefaceVerdict::NeedMore
    };
    // H2 exactly when the whole preface was seen and matched.
    debug_assert_eq!(
        verdict == PrefaceVerdict::H2,
        seen == H2C_PREFACE_LEN && peeked[..seen] == CLIENT_PREFACE_MAGIC[..],
        "H2 iff all {H2C_PREFACE_LEN} preface bytes were seen and matched"
    );
    // NeedMore never hides a divergence, and never fires on a full preface.
    debug_assert!(
        verdict != PrefaceVerdict::NeedMore
            || (seen < H2C_PREFACE_LEN && peeked[..seen] == CLIENT_PREFACE_MAGIC[..seen]),
        "NeedMore only for a strict, matching prefix of the preface"
    );
    verdict
}

/// The session stage of an `h2c_prior_knowledge` HTTP listener that decides
/// between HTTP/2 and HTTP/1.x before the mux is built. See the module
/// documentation.
pub struct H2cPrefaceSniff {
    pub container_frontend_timeout: TimeoutContainer,
    pub frontend: TcpStream,
    pub frontend_readiness: Readiness,
    pub frontend_token: Token,
    /// Address the session is attributed to: the `accept(2)` peer, or the
    /// PROXY-advertised source when the listener also expects PROXY.
    pub session_address: Option<SocketAddr>,
    /// Address the session was received on, as the mux context records it.
    pub public_address: SocketAddr,
    /// Request id of the first HTTP/1.x request, minted at accept.
    pub request_id: Ulid,
    /// `Some(H2 | Http1)` once decided; never `Some(NeedMore)`.
    pub verdict: Option<PrefaceVerdict>,
}

impl H2cPrefaceSniff {
    /// Instantiate the sniff stage with interest READABLE | HUP | ERROR,
    /// carrying `event` (the readiness already observed on the socket, EMPTY
    /// for a freshly accepted one).
    pub fn new(
        container_frontend_timeout: TimeoutContainer,
        frontend: TcpStream,
        frontend_token: Token,
        request_id: Ulid,
        session_address: Option<SocketAddr>,
        public_address: SocketAddr,
        event: Ready,
    ) -> Self {
        H2cPrefaceSniff {
            container_frontend_timeout,
            frontend,
            frontend_readiness: Readiness {
                interest: Ready::READABLE | Ready::HUP | Ready::ERROR,
                event,
            },
            frontend_token,
            session_address,
            public_address,
            request_id,
            verdict: None,
        }
    }

    pub fn front_socket(&self) -> &TcpStream {
        &self.frontend
    }

    /// Peek at the socket and try to decide. Returns `Upgrade` once decided,
    /// `Continue` to wait for more bytes, `Close` on end of stream or error.
    fn readable(&mut self) -> SessionResult {
        debug_assert!(
            self.verdict.is_none(),
            "the sniff must not peek again after it decided"
        );
        let mut window = [0u8; H2C_PREFACE_LEN];
        match self.frontend.peek(&mut window) {
            Ok(0) => {
                // The client closed before sending a byte: a bare TCP health
                // check. Close now rather than at the request timeout.
                trace!(
                    "{} socket closed with 0 bytes, closing session",
                    log_context!(self)
                );
                SessionResult::Close
            }
            Ok(peeked) => {
                debug_assert!(
                    peeked <= H2C_PREFACE_LEN,
                    "peek cannot return more than the window it was given"
                );
                match classify_preface(&window[..peeked]) {
                    PrefaceVerdict::NeedMore => {
                        trace!(
                            "{} {} of {} preface bytes so far, waiting for more",
                            log_context!(self),
                            peeked,
                            H2C_PREFACE_LEN
                        );
                        // Nothing more to see until the next segment, which
                        // raises its own edge.
                        self.frontend_readiness.event.remove(Ready::READABLE);
                        SessionResult::Continue
                    }
                    verdict => {
                        debug!(
                            "{} connection preface verdict: {:?}",
                            log_context!(self),
                            verdict
                        );
                        if verdict == PrefaceVerdict::H2 {
                            incr!(names::http::H2C_PRIOR_KNOWLEDGE);
                        }
                        self.verdict = Some(verdict);
                        SessionResult::Upgrade
                    }
                }
            }
            Err(e) if e.kind() == ErrorKind::WouldBlock => {
                self.frontend_readiness.event.remove(Ready::READABLE);
                SessionResult::Continue
            }
            // Retried by the next turn of the bounded `ready` loop: READABLE
            // is still set.
            Err(e) if e.kind() == ErrorKind::Interrupted => SessionResult::Continue,
            Err(e) => {
                debug!(
                    "{} front socket peek error, closing the connection: {:?}",
                    log_context!(self),
                    e
                );
                self.frontend_readiness.reset();
                SessionResult::Close
            }
        }
    }
}

impl SessionState for H2cPrefaceSniff {
    fn ready(
        &mut self,
        _session: Rc<RefCell<dyn ProxySession>>,
        _proxy: Rc<RefCell<dyn L7Proxy>>,
        _metrics: &mut SessionMetrics,
    ) -> SessionResult {
        let mut counter = 0;

        while counter < MAX_LOOP_ITERATIONS {
            let frontend_interest = self.frontend_readiness.filter_interest();

            trace!("{} {:?}", log_context!(self), self.frontend_readiness);

            // A client half-close arrives in the same `READABLE | HUP` event
            // as the bytes it follows, so HUP closes only once a peek has
            // looked at them: a complete preface or a diverging byte still
            // upgrades and hands HUP on to the mux. `readable` clears
            // READABLE on a strict prefix, so a client that half-closes
            // mid-preface reaches this close on the next turn.
            if self.frontend_readiness.event.is_hup() && !frontend_interest.is_readable() {
                return SessionResult::Close;
            }

            if frontend_interest.is_empty() {
                break;
            }

            if frontend_interest.is_readable() {
                let session_result = self.readable();
                if session_result != SessionResult::Continue {
                    // Upgrade carries a decision, Close none.
                    debug_assert_eq!(
                        session_result == SessionResult::Upgrade,
                        self.verdict.is_some(),
                        "the sniff upgrades exactly when it has decided"
                    );
                    return session_result;
                }
            }

            if frontend_interest.is_error() {
                error!("{} front error, disconnecting", log_context!(self));
                self.frontend_readiness.interest = Ready::EMPTY;
                return SessionResult::Close;
            }

            counter += 1;
        }

        if counter >= MAX_LOOP_ITERATIONS {
            error!(
                "{} handling session went through {} iterations, there's a probable infinite loop bug, closing the connection",
                log_context!(self),
                MAX_LOOP_ITERATIONS
            );
            incr!(names::http::INFINITE_LOOP_ERROR);
            self.print_state("");
            return SessionResult::Close;
        }

        debug_assert!(
            self.verdict.is_none(),
            "an undecided sniff is the only one that keeps waiting"
        );
        SessionResult::Continue
    }

    fn update_readiness(&mut self, token: Token, events: Ready) {
        if self.frontend_token == token {
            self.frontend_readiness.event |= events;
        }
    }

    fn timeout(&mut self, token: Token, _metrics: &mut SessionMetrics) -> StateResult {
        if self.frontend_token == token {
            debug!(
                "{} request timeout before the connection preface was decided",
                log_context!(self)
            );
            self.container_frontend_timeout.triggered();
            return StateResult::CloseSession;
        }

        error!(
            "{} got timeout for an invalid token: {:?}",
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
            "{} {} Session(H2cPrefaceSniff)\n\tFrontend:\n\t\ttoken: {:?}\treadiness: {:?}\tverdict: {:?}",
            log_context!(self),
            context,
            self.frontend_token,
            self.frontend_readiness,
            self.verdict
        );
    }
}

#[cfg(test)]
mod tests {
    use std::{
        io::Write,
        net::{SocketAddr, TcpStream as StdTcpStream},
        time::{Duration, Instant},
    };

    use mio::net::TcpListener;

    use super::*;

    #[test]
    fn full_preface_is_h2() {
        assert_eq!(
            classify_preface(&CLIENT_PREFACE_MAGIC[..]),
            PrefaceVerdict::H2
        );
    }

    #[test]
    fn preface_followed_by_settings_is_h2() {
        let mut wire = CLIENT_PREFACE_MAGIC.to_vec();
        // An empty SETTINGS frame header: what every client sends next.
        wire.extend_from_slice(&[0, 0, 0, 4, 0, 0, 0, 0, 0]);
        assert_eq!(classify_preface(&wire), PrefaceVerdict::H2);
    }

    #[test]
    fn every_strict_prefix_needs_more() {
        for len in 0..H2C_PREFACE_LEN {
            assert_eq!(
                classify_preface(&CLIENT_PREFACE_MAGIC[..len]),
                PrefaceVerdict::NeedMore,
                "a {len}-byte prefix of the preface must wait for more"
            );
        }
    }

    #[test]
    fn a_divergence_at_any_byte_is_http1() {
        for at in 0..H2C_PREFACE_LEN {
            let mut wire = CLIENT_PREFACE_MAGIC.to_vec();
            wire[at] ^= 0x20;
            // The divergence decides as soon as it is seen, short or full.
            for len in (at + 1)..=H2C_PREFACE_LEN {
                assert_eq!(
                    classify_preface(&wire[..len]),
                    PrefaceVerdict::Http1,
                    "a divergence at byte {at} must decide HTTP/1.x after {len} bytes"
                );
            }
        }
    }

    #[test]
    fn http1_requests_are_http1() {
        for request in [
            &b"GET / HTTP/1.1\r\nHost: localhost\r\n\r\n"[..],
            b"POST /upload HTTP/1.1\r\n",
            b"PUT",
            b"PRI * HTTP/1.1\r\n\r\n",
            b"PRI / HTTP/2.0\r\n\r\nSM\r\n\r\n",
        ] {
            assert_eq!(
                classify_preface(request),
                PrefaceVerdict::Http1,
                "{:?} must be served as HTTP/1.x",
                String::from_utf8_lossy(request)
            );
        }
    }

    /// Loopback pair: the accepted, non-blocking mio side and the client.
    fn connected_pair() -> (TcpStream, StdTcpStream) {
        let listener = TcpListener::bind("127.0.0.1:0".parse::<SocketAddr>().unwrap())
            .expect("bind loopback listener");
        let client = StdTcpStream::connect(listener.local_addr().unwrap())
            .expect("connect to loopback listener");
        let deadline = Instant::now() + Duration::from_secs(5);
        loop {
            match listener.accept() {
                Ok((server, _)) => return (server, client),
                Err(e) if e.kind() == ErrorKind::WouldBlock && Instant::now() < deadline => {
                    std::thread::sleep(Duration::from_millis(5));
                }
                Err(e) => panic!("accept failed: {e}"),
            }
        }
    }

    fn sniff_over(frontend: TcpStream) -> H2cPrefaceSniff {
        H2cPrefaceSniff::new(
            TimeoutContainer::new_empty(Duration::from_secs(10)),
            frontend,
            Token(1),
            Ulid::generate(),
            None,
            "127.0.0.1:80".parse().unwrap(),
            Ready::READABLE,
        )
    }

    /// Peek until the kernel shows at least `want` bytes, then classify once.
    fn readable_after(sniff: &mut H2cPrefaceSniff, want: usize) -> SessionResult {
        let deadline = Instant::now() + Duration::from_secs(5);
        let mut probe = [0u8; H2C_PREFACE_LEN];
        while sniff.frontend.peek(&mut probe).unwrap_or(0) < want {
            assert!(Instant::now() < deadline, "bytes never arrived");
            std::thread::sleep(Duration::from_millis(5));
        }
        sniff.frontend_readiness.event.insert(Ready::READABLE);
        sniff.readable()
    }

    /// The sniff decides on a split preface without consuming a byte: the
    /// mux built next must read the preface itself.
    #[test]
    fn a_split_preface_waits_then_decides_h2_without_consuming() {
        let (frontend, mut client) = connected_pair();
        let mut sniff = sniff_over(frontend);

        client.write_all(&CLIENT_PREFACE_MAGIC[..10]).unwrap();
        assert_eq!(readable_after(&mut sniff, 10), SessionResult::Continue);
        assert_eq!(sniff.verdict, None);
        assert!(
            !sniff.frontend_readiness.event.is_readable(),
            "a strict prefix must park READABLE until the next edge"
        );

        client.write_all(&CLIENT_PREFACE_MAGIC[10..]).unwrap();
        assert_eq!(
            readable_after(&mut sniff, H2C_PREFACE_LEN),
            SessionResult::Upgrade
        );
        assert_eq!(sniff.verdict, Some(PrefaceVerdict::H2));

        let mut left = [0u8; H2C_PREFACE_LEN];
        assert_eq!(
            sniff.frontend.peek(&mut left).unwrap(),
            H2C_PREFACE_LEN,
            "the sniff must leave every preface byte in the socket"
        );
        assert_eq!(&left, CLIENT_PREFACE_MAGIC);
    }

    #[test]
    fn an_http1_request_decides_http1_without_consuming() {
        let (frontend, mut client) = connected_pair();
        let mut sniff = sniff_over(frontend);

        let request = b"GET / HTTP/1.1\r\nHost: localhost\r\n\r\n";
        client.write_all(request).unwrap();
        assert_eq!(readable_after(&mut sniff, 1), SessionResult::Upgrade);
        assert_eq!(sniff.verdict, Some(PrefaceVerdict::Http1));

        let mut left = vec![0u8; request.len()];
        let deadline = Instant::now() + Duration::from_secs(5);
        while sniff.frontend.peek(&mut left).unwrap() < request.len() {
            assert!(Instant::now() < deadline, "request bytes never arrived");
            std::thread::sleep(Duration::from_millis(5));
        }
        assert_eq!(&left[..], &request[..]);
    }

    #[test]
    fn a_close_before_any_byte_closes_the_session() {
        let (frontend, client) = connected_pair();
        let mut sniff = sniff_over(frontend);
        drop(client);

        let deadline = Instant::now() + Duration::from_secs(5);
        let result = loop {
            sniff.frontend_readiness.event.insert(Ready::READABLE);
            match sniff.readable() {
                SessionResult::Continue if Instant::now() < deadline => {
                    std::thread::sleep(Duration::from_millis(5));
                }
                other => break other,
            }
        };
        assert_eq!(result, SessionResult::Close);
        assert_eq!(sniff.verdict, None);
    }
}
