//! Pure sans-io TCP passthrough SNI-preread core (issue #1279).
//!
//! [`SniPrereadCore`] decides, from the raw bytes of an inbound TLS
//! ClientHello, which cluster a TCP passthrough connection routes to --
//! WITHOUT terminating TLS. It performs **no I/O**: there is no socket, no
//! `Instant::now()`, no `rand`, and no `Arc<Mutex>`. Time is injected as
//! `now: Instant` parameters ([`Input::Bytes`] / [`Input::Timeout`]); routing
//! config is injected borrowed ([`PrereadConfig`]). It never mutates,
//! consumes, or re-serializes the caller's buffer -- byte-for-byte replay is
//! the whole point: once routed, the shell forwards the SAME bytes verbatim
//! to the backend, starting at [`Output::Routed::content_offset`].
//!
//! Mirrors the split already established by [`crate::protocol::udp`]: a
//! near-stateless core (`decided` latch + `deadline`, see
//! [`SniPrereadCore`]) driven by [`Input`] / [`Output`] through
//! [`SniPrereadCore::handle_input`], with the accumulating byte buffer owned
//! by the I/O shell ([`shell::SniPreread`], wired into the session lifecycle
//! by `lib/src/tcp.rs`) rather than by the core itself -- every
//! [`Input::Bytes`] carries the FULL accumulated window from wire offset 0,
//! not a delta.
//!
//! [`parser`] owns the nom-based wire format (TLS record layer, ClientHello,
//! and extensions); this module owns the PROXY-v2 stripping, the SNI/ALPN
//! routing decision against [`crate::router::pattern_trie::TrieNode`], and
//! the decided/deadline state machine.

mod parser;
pub mod shell;

use std::{
    collections::BTreeSet,
    net::SocketAddr,
    time::{Duration, Instant},
};

use nom::Err as NomErr;
use sozu_command::state::ClusterId;

use crate::{protocol::proxy_protocol::parser::parse_v2_header, router::pattern_trie::TrieNode};

/// How a route entry's cluster accepts the client's ALPN offer.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum AlpnMatcher {
    /// Matches regardless of what the client offered (including a client
    /// that sent no `alpn` extension at all). The catch-all: fires only
    /// once no `OneOf` entry matched any client-offered protocol.
    Any,
    /// Matches when at least one of the client's offered protocols is a
    /// member of this set.
    OneOf(BTreeSet<Vec<u8>>),
}

/// Routing input, borrowed for the lifetime of one [`SniPrereadCore::handle_input`]
/// call. `'t` is the lifetime of the route table itself (owned by the
/// listener config, well outside any single preread attempt).
#[derive(Debug, Clone, Copy)]
pub struct PrereadConfig<'t> {
    /// SNI -> `(AlpnMatcher, ClusterId)` route table, reusing
    /// [`crate::router::pattern_trie::TrieNode`] (exact hosts, `*.`
    /// wildcards, regex segments -- same trie the HTTP/HTTPS router uses).
    pub routes: &'t TrieNode<Vec<(AlpnMatcher, ClusterId)>>,
    /// Whether a PROXY-v2 header precedes the ClientHello on the wire.
    pub inbound_proxy: bool,
    /// Effective byte cap for the preread window, enforced only while the
    /// decision is still pending: a window that reaches `max_bytes` without
    /// a complete PROXY header + ClientHello rejects `TooLarge`; a COMPLETE
    /// hello routes regardless of trailing post-hello bytes. The caller
    /// guarantees this is `<=` the accumulator buffer's capacity.
    pub max_bytes: usize,
    /// Preread deadline, armed once from the first `Input::Bytes`.
    pub timeout: Duration,
    /// Whether a `*.` wildcard route may satisfy a lookup (mirrors the
    /// HTTP router's `accept_wildcard` knob on
    /// [`TrieNode::domain_lookup`]).
    pub accept_wildcard: bool,
}

/// One input fed into [`SniPrereadCore::handle_input`].
#[derive(Debug)]
pub enum Input<'a> {
    /// The FULL accumulated preread window, from wire offset 0, re-fed on
    /// every call (never a delta). `now` drives the one-time deadline arm.
    Bytes { buf: &'a [u8], now: Instant },
    /// The preread deadline fired without a decision.
    Timeout { now: Instant },
    /// The frontend socket closed before a decision was reached.
    FrontClosed,
}

/// One outcome of [`SniPrereadCore::handle_input`].
#[derive(Debug, Clone, PartialEq)]
pub enum Output {
    /// Not enough bytes yet; `deadline` is the same value for the whole
    /// lifetime of one [`SniPrereadCore`] (armed once, on the first
    /// `Bytes` input).
    NeedMore { deadline: Instant },
    /// A cluster was chosen. `content_offset` is how many leading bytes of
    /// the ORIGINAL buffer are the (already-parsed) PROXY-v2 header -- the
    /// shell must skip exactly that many bytes before forwarding the
    /// UNTOUCHED remainder to the backend verbatim.
    Routed {
        cluster: ClusterId,
        content_offset: usize,
        proxy_source: Option<SocketAddr>,
        sni: String,
        alpn: Vec<Vec<u8>>,
        /// The trie KEY that matched -- the route's configured pattern, not
        /// the client's concrete SNI: for a wildcard route this is
        /// `*.example.com` even when `sni` is `a.example.com`. Carries the
        /// matched route's identity so the shell/session can key
        /// per-frontend state (e.g. access-log tags) without re-running the
        /// lookup.
        matched_sni_pattern: String,
        /// Clone of the winning route entry's [`AlpnMatcher`] -- the other
        /// half of the matched route's identity.
        matched_alpn: AlpnMatcher,
    },
    /// Terminal, non-routable verdict. See [`RejectReason`].
    Reject(RejectReason),
}

/// Why a preread attempt was terminally rejected.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RejectReason {
    /// The first TLS record's `ContentType` was not `handshake` (22).
    NotTls,
    /// A TLS record's framing is invalid (bad length, or a non-`handshake`
    /// record interrupting an in-progress ClientHello reassembly).
    MalformedRecord,
    /// The handshake message is not a ClientHello, a length-prefixed field
    /// inside it lies about the bytes available, or the `server_name`
    /// extension's `host_name` is not valid UTF-8.
    MalformedHandshake,
    /// The preread deadline fired before a terminal verdict was reached.
    Fragmented,
    /// The accumulated window reached [`PrereadConfig::max_bytes`] while
    /// the PROXY header / ClientHello was still incomplete -- it can never
    /// complete within the cap. A COMPLETE hello routes regardless of total
    /// window length (post-hello trailing bytes are normal in passthrough).
    TooLarge,
    /// No usable SNI: the `server_name` extension was absent, empty, or
    /// its RFC 6066 `host_name` entry was missing.
    NoSni,
    /// `encrypted_client_hello` (`0xfe0d`) was present and no usable outer
    /// `server_name` was available -- distinguished from [`Self::NoSni`]
    /// because an ECH-hiding client is a different operational signal.
    EchOuterAbsent,
    /// The normalized SNI matched no route.
    SniUnmatched,
    /// The SNI matched a route, but no entry's [`AlpnMatcher`] accepted the
    /// client's offered ALPN protocols (and no [`AlpnMatcher::Any`] entry
    /// was present either).
    AlpnUnmatched,
    /// The PROXY-v2 header itself failed to parse.
    ProxyHeaderInvalid,
    /// The frontend closed before a decision was reached.
    FrontClosed,
}

/// The pure preread core. Deliberately near-stateless: the shell owns the
/// growing byte accumulator and re-feeds the FULL window on every
/// [`Input::Bytes`]; this struct only remembers whether a terminal verdict
/// has already been latched (`decided`) and when the preread deadline was
/// armed (`deadline`).
#[derive(Debug, Default)]
pub struct SniPrereadCore {
    /// Set exactly once, by [`Self::decide`], to the first terminal
    /// [`Output::Routed`] / [`Output::Reject`]. Monotonic: once `Some`, it
    /// never changes and every subsequent [`Self::handle_input`] call
    /// replays it verbatim without re-parsing anything.
    decided: Option<Output>,
    /// Set exactly once, on the first [`Input::Bytes`], to `now +
    /// cfg.timeout`. Never rewound.
    deadline: Option<Instant>,
}

impl SniPrereadCore {
    /// Construct a fresh, undecided core.
    pub fn new() -> Self {
        SniPrereadCore {
            decided: None,
            deadline: None,
        }
    }

    /// Feed one input. Pure: `now` (inside [`Input`]) and `cfg` are
    /// injected, never read from ambient state.
    pub fn handle_input(&mut self, cfg: &PrereadConfig<'_>, input: Input<'_>) -> Output {
        #[cfg(debug_assertions)]
        let deadline_before = self.deadline;
        #[cfg(debug_assertions)]
        let was_decided = self.decided.is_some();
        self.debug_assert_invariants();

        let output = match input {
            Input::Bytes { buf, now } => self.on_bytes(cfg, buf, now),
            Input::Timeout { now } => self.on_timeout(now),
            Input::FrontClosed => self.on_front_closed(),
        };

        // Post-conditions specific to this call: the deadline, once armed,
        // never rewinds; a core that was already decided must still be
        // decided (the latch never reverts to undecided).
        #[cfg(debug_assertions)]
        {
            if let Some(before) = deadline_before {
                debug_assert_eq!(
                    self.deadline,
                    Some(before),
                    "preread deadline must be set at most once and never rewound"
                );
            }
            debug_assert!(
                !was_decided || self.decided.is_some(),
                "a decided core must never become undecided (monotonic latch)"
            );
        }
        self.debug_assert_invariants();

        output
    }

    fn on_bytes(&mut self, cfg: &PrereadConfig<'_>, buf: &[u8], now: Instant) -> Output {
        if self.deadline.is_none() {
            self.deadline = Some(now + cfg.timeout);
        }
        let deadline = self
            .deadline
            .expect("deadline was just armed unconditionally above if it was None");

        if let Some(decided) = &self.decided {
            return decided.clone();
        }

        // NOTE: the `max_bytes` cap is deliberately NOT checked here. In
        // passthrough the client keeps sending after its hello, so one read
        // routinely delivers a complete ClientHello PLUS trailing bytes that
        // push the window over the cap -- a complete hello must still route
        // (regression guards: `complete_hello_with_trailing_bytes_over_cap_routes`
        // below, plus the e2e coalesced-read test
        // `test_tcp_sni_large_payload_coalesced_with_hello_delivered_intact`).
        // The cap gates only the would-be-NeedMore paths below, via
        // [`Self::need_more_or_too_large`].
        let (content_offset, proxy_source) = if cfg.inbound_proxy {
            match parse_v2_header(buf) {
                Ok((rest, header)) => {
                    let consumed = buf.len() - rest.len();
                    debug_assert!(
                        consumed <= buf.len(),
                        "a parsed PROXY-v2 header cannot consume more than the buffer holds"
                    );
                    (consumed, header.addr.source())
                }
                Err(NomErr::Incomplete(_)) => {
                    return self.need_more_or_too_large(cfg, buf, deadline);
                }
                Err(_) => return self.decide(Output::Reject(RejectReason::ProxyHeaderInvalid)),
            }
        } else {
            (0, None)
        };

        match parser::parse_client_hello(&buf[content_offset..]) {
            parser::ParseOutcome::NeedMore => self.need_more_or_too_large(cfg, buf, deadline),
            parser::ParseOutcome::Reject(reason) => self.decide(Output::Reject(reason)),
            parser::ParseOutcome::ClientHello {
                sni,
                alpn,
                ech_present,
            } => self.route(
                cfg,
                buf,
                content_offset,
                proxy_source,
                sni,
                alpn,
                ech_present,
            ),
        }
    }

    fn on_timeout(&mut self, _now: Instant) -> Output {
        // `now` is accepted for signature symmetry with `on_bytes`; once
        // the deadline has fired, the fragmented-timeout verdict itself is
        // time-independent.
        if let Some(decided) = &self.decided {
            return decided.clone();
        }
        self.decide(Output::Reject(RejectReason::Fragmented))
    }

    fn on_front_closed(&mut self) -> Output {
        if let Some(decided) = &self.decided {
            return decided.clone();
        }
        self.decide(Output::Reject(RejectReason::FrontClosed))
    }

    /// The byte cap, applied where the outcome would otherwise be
    /// [`Output::NeedMore`]: a window that has already reached `max_bytes`
    /// without yielding a complete PROXY header + ClientHello can never
    /// grow into one within the cap, so waiting is pointless -- reject
    /// `TooLarge` now. A COMPLETE hello never reaches this path and routes
    /// regardless of total window length.
    fn need_more_or_too_large(
        &mut self,
        cfg: &PrereadConfig<'_>,
        buf: &[u8],
        deadline: Instant,
    ) -> Output {
        if buf.len() >= cfg.max_bytes {
            return self.decide(Output::Reject(RejectReason::TooLarge));
        }
        Output::NeedMore { deadline }
    }

    /// Normalize the extracted SNI, look it up, and resolve ALPN against
    /// the matched route's entries.
    #[allow(clippy::too_many_arguments)]
    fn route(
        &mut self,
        cfg: &PrereadConfig<'_>,
        buf: &[u8],
        content_offset: usize,
        proxy_source: Option<SocketAddr>,
        sni_raw: Option<String>,
        alpn: Vec<Vec<u8>>,
        ech_present: bool,
    ) -> Output {
        let sni = sni_raw.map(normalize_sni);
        let sni = match sni {
            Some(s) if !s.is_empty() => s,
            _ => {
                let reason = if ech_present {
                    RejectReason::EchOuterAbsent
                } else {
                    RejectReason::NoSni
                };
                return self.decide(Output::Reject(reason));
            }
        };

        let Some((matched_key, entries)) = cfg
            .routes
            .domain_lookup(sni.as_bytes(), cfg.accept_wildcard)
        else {
            return self.decide(Output::Reject(RejectReason::SniUnmatched));
        };

        let Some((matched_alpn, cluster)) = resolve_alpn(entries, &alpn) else {
            return self.decide(Output::Reject(RejectReason::AlpnUnmatched));
        };

        // Route determinism (pair): re-running the SAME lookup + ALPN
        // resolution must yield the SAME entry -- routing is a pure
        // function of (sni, accept_wildcard, alpn) at a given instant, so a
        // divergence here means the trie or `resolve_alpn` has a hidden
        // side effect or non-determinism (e.g. iterating a HashMap without
        // a stable order).
        #[cfg(debug_assertions)]
        {
            let (_, replay_entries) = cfg
                .routes
                .domain_lookup(sni.as_bytes(), cfg.accept_wildcard)
                .expect("a route that just matched must match again");
            let replay_entry = resolve_alpn(replay_entries, &alpn);
            debug_assert_eq!(
                replay_entry,
                Some((matched_alpn, cluster)),
                "route lookup + ALPN resolution must be deterministic for the same (sni, alpn)"
            );
        }
        debug_assert!(
            content_offset <= buf.len(),
            "content_offset must never exceed the buffer it was derived from"
        );

        self.decide(Output::Routed {
            cluster: cluster.to_owned(),
            content_offset,
            proxy_source,
            sni,
            alpn,
            // The trie key is the route's configured pattern verbatim
            // (lowercased at insert); `from_utf8_lossy` is defensive -- keys
            // originate from config/IPC `String`s, never raw network bytes.
            matched_sni_pattern: String::from_utf8_lossy(matched_key).into_owned(),
            matched_alpn: matched_alpn.clone(),
        })
    }

    /// Latch the first (and only) terminal verdict. Every subsequent input
    /// replays it via the `self.decided` checks in `on_*` instead of
    /// calling this again.
    fn decide(&mut self, output: Output) -> Output {
        debug_assert!(
            self.decided.is_none(),
            "decide() must be called at most once per lifetime -- the latch is monotonic"
        );
        debug_assert!(
            !matches!(output, Output::NeedMore { .. }),
            "only a terminal Output (Routed/Reject) may be latched via decide()"
        );
        self.decided = Some(output.clone());
        debug!(
            "{} latched terminal preread decision: {:?}",
            log_context!(self),
            output
        );
        output
    }

    /// TigerStyle invariant sweep, run at both the head and tail of
    /// [`Self::handle_input`] (mirrors `udp/manager.rs`'s
    /// `check_invariants`, see `lib/src/protocol/udp/manager.rs:686-817`).
    /// Read-only; must never change behavior.
    #[cfg(debug_assertions)]
    fn check_invariants(&self) {
        // Positive: `NeedMore` is the only non-terminal `Output` variant, so
        // the decided latch -- which only ever stores a TERMINAL verdict --
        // must never hold one.
        debug_assert!(
            !matches!(self.decided, Some(Output::NeedMore { .. })),
            "decided latch must never store NeedMore -- it is not a terminal verdict"
        );
        // Pair (positive + negative space): a latched SNI is always already
        // normalized -- lowercase, no trailing dot -- because `route` never
        // stores the raw wire-form SNI. The matched pattern is the trie KEY
        // (lowercased at insert): either the concrete SNI itself (exact
        // route) or a `*.`-prefixed wildcard whose suffix the SNI ends with.
        if let Some(Output::Routed {
            sni,
            matched_sni_pattern,
            ..
        }) = &self.decided
        {
            debug_assert_eq!(
                sni,
                &sni.to_ascii_lowercase(),
                "latched SNI must already be lowercase-normalized"
            );
            debug_assert!(
                !sni.ends_with('.'),
                "latched SNI must not carry a trailing dot after normalization"
            );
            debug_assert_eq!(
                matched_sni_pattern,
                &matched_sni_pattern.to_ascii_lowercase(),
                "latched matched_sni_pattern must already be lowercase (trie keys are lowercased at insert)"
            );
            debug_assert!(
                matched_sni_pattern == sni
                    || matched_sni_pattern
                        .strip_prefix("*.")
                        .is_some_and(|suffix| sni.ends_with(suffix)),
                "matched_sni_pattern must be the SNI itself (exact route) or a wildcard the SNI falls under"
            );
        }
    }

    #[inline]
    fn debug_assert_invariants(&self) {
        #[cfg(debug_assertions)]
        self.check_invariants();
    }
}

/// Resolve a cluster from a matched route's `(AlpnMatcher, ClusterId)`
/// entries against the client's offered ALPN protocols (in client
/// preference order).
///
/// Walks the CLIENT's offer first (outer loop), so the first offered
/// protocol that is a member of ANY entry's [`AlpnMatcher::OneOf`] wins --
/// client preference order beats route entry order. Only once no offered
/// protocol matched anything (including a client that offered no ALPN at
/// all) does an [`AlpnMatcher::Any`] entry, if present, win as the
/// catch-all.
fn resolve_alpn<'r>(
    entries: &'r [(AlpnMatcher, ClusterId)],
    offered: &[Vec<u8>],
) -> Option<(&'r AlpnMatcher, &'r ClusterId)> {
    for protocol in offered {
        for (matcher, cluster) in entries {
            if let AlpnMatcher::OneOf(set) = matcher
                && set.contains(protocol)
            {
                return Some((matcher, cluster));
            }
        }
    }
    entries
        .iter()
        .find(|(matcher, _)| matches!(matcher, AlpnMatcher::Any))
        .map(|(matcher, cluster)| (matcher, cluster))
}

/// Normalize a wire-form SNI: ASCII-lowercase, then strip AT MOST one
/// trailing dot (RFC 1034 §3.1 absolute-form: `example.com.` and
/// `example.com` are the same host). Mirrors `lib/src/https.rs`'s
/// `upgrade_handshake` SNI normalization so a TCP-preread route decision
/// and the eventual TLS-terminating one agree on the same host.
fn normalize_sni(mut sni: String) -> String {
    sni.make_ascii_lowercase();
    if sni.ends_with('.') {
        sni.pop();
    }
    sni
}

/// Logging prefix for the pure TCP-preread core (tag `TCP-SNI`). Mirrors
/// `protocol/udp/mod.rs`'s `log_context!`; honours the colored flag via
/// [`ansi_palette`](sozu_command::logging::ansi_palette). The core has no
/// socket of its own, so the prefix carries the decided/deadline latch
/// state instead of a session id.
#[allow(unused_macros)]
macro_rules! log_context {
    ($self:expr) => {{
        let (open, reset, grey, gray, white) = sozu_command::logging::ansi_palette();
        format!(
            "[- - - -]\t{open}TCP-SNI{reset}\t{grey}Preread{reset}({gray}decided{reset}={white}{decided}{reset}, {gray}deadline_armed{reset}={white}{deadline}{reset})\t >>>",
            open = open,
            reset = reset,
            grey = grey,
            gray = gray,
            white = white,
            decided = $self.decided.is_some(),
            deadline = $self.deadline.is_some(),
        )
    }};
}

/// Lightweight per-decision logging prefix (tag `TCP-SNI`), for call sites
/// that only have the resolved SNI in scope, not a full `&SniPrereadCore`.
/// Mirrors `protocol/udp/mod.rs`'s `log_context_lite!`; a pure core this
/// small barely logs, so this macro currently has no call site of its own
/// (kept for parity with the mirrored module and for future shell-side use).
#[allow(unused_macros)]
macro_rules! log_context_lite {
    ($sni:expr) => {{
        let (open, reset, grey, gray, white) = sozu_command::logging::ansi_palette();
        format!(
            "[- - - -]\t{open}TCP-SNI{reset}\t{grey}Route{reset}({gray}sni{reset}={white}{sni:?}{reset})\t >>>",
            open = open,
            reset = reset,
            grey = grey,
            gray = gray,
            white = white,
            sni = $sni,
        )
    }};
}

#[allow(unused_imports)]
use {log_context, log_context_lite};

#[cfg(test)]
mod tests {
    use std::net::{IpAddr, Ipv4Addr};

    use super::*;
    use crate::protocol::proxy_protocol::header::{Command, HeaderV2};

    fn cfg<'t>(routes: &'t TrieNode<Vec<(AlpnMatcher, ClusterId)>>) -> PrereadConfig<'t> {
        PrereadConfig {
            routes,
            inbound_proxy: false,
            max_bytes: 16 * 1024,
            timeout: Duration::from_secs(3),
            accept_wildcard: true,
        }
    }

    fn one_of(protocols: &[&[u8]]) -> AlpnMatcher {
        AlpnMatcher::OneOf(protocols.iter().map(|p| p.to_vec()).collect())
    }

    fn hello(sni: &str, alpn: &[&[u8]]) -> Vec<u8> {
        parser::build_client_hello_wire(&[
            parser::encode_sni_extension(sni),
            parser::encode_alpn_extension(alpn),
        ])
    }

    fn hello_no_alpn(sni: &str) -> Vec<u8> {
        parser::build_client_hello_wire(&[parser::encode_sni_extension(sni)])
    }

    fn feed(core: &mut SniPrereadCore, cfg: &PrereadConfig<'_>, buf: &[u8]) -> Output {
        core.handle_input(
            cfg,
            Input::Bytes {
                buf,
                now: Instant::now(),
            },
        )
    }

    // ---- byte-replay + core-level RejectReason reachability ----------------

    #[test]
    fn byte_replay_untouched_through_the_core() {
        let mut routes = TrieNode::root();
        routes.domain_insert(
            b"example.com".to_vec(),
            vec![(AlpnMatcher::Any, "cluster-a".to_owned())],
        );
        let cfg = cfg(&routes);
        let wire = hello("example.com", &[]);
        let before = wire.clone();
        let mut core = SniPrereadCore::new();
        let _ = feed(&mut core, &cfg, &wire);
        assert_eq!(wire, before, "the core must never mutate the fed buffer");
    }

    /// The genuine `TooLarge` case: an INCOMPLETE hello whose accumulated
    /// window has grown past the cap can never complete within it.
    #[test]
    fn too_large_is_reachable() {
        let routes = TrieNode::root();
        let mut small_cfg = cfg(&routes);
        small_cfg.max_bytes = 8;
        let wire = hello_no_alpn("example.com");
        let mut core = SniPrereadCore::new();
        // 10 bytes of a real hello: valid record header, body incomplete.
        let out = feed(&mut core, &small_cfg, &wire[..10]);
        assert_eq!(out, Output::Reject(RejectReason::TooLarge));
    }

    /// Cap boundary, both sides: a COMPLETE hello whose total length is
    /// exactly `max_bytes` routes; an INCOMPLETE window that has reached
    /// exactly `max_bytes` can never complete within the cap -- `TooLarge`.
    #[test]
    fn exact_cap_boundary_complete_routes_incomplete_rejects() {
        let mut routes = TrieNode::root();
        routes.domain_insert(
            b"example.com".to_vec(),
            vec![(AlpnMatcher::Any, "cluster-a".to_owned())],
        );
        let wire = hello_no_alpn("example.com");

        let mut exact_cfg = cfg(&routes);
        exact_cfg.max_bytes = wire.len();
        let mut core = SniPrereadCore::new();
        match feed(&mut core, &exact_cfg, &wire) {
            Output::Routed { cluster, .. } => assert_eq!(cluster, "cluster-a"),
            other => panic!("expected Routed at exactly the cap, got {other:?}"),
        }

        let mut tight_cfg = cfg(&routes);
        tight_cfg.max_bytes = wire.len() - 1;
        let mut core = SniPrereadCore::new();
        assert_eq!(
            feed(&mut core, &tight_cfg, &wire[..wire.len() - 1]),
            Output::Reject(RejectReason::TooLarge),
            "an incomplete window at exactly the cap must reject TooLarge"
        );
    }

    /// Regression, surfaced by the e2e coalesced-read test
    /// (`test_tcp_sni_large_payload_coalesced_with_hello_delivered_intact`
    /// in `e2e/src/tests/tcp_sni_tests.rs`): in TLS passthrough the client
    /// keeps sending after its hello (rest of handshake / early data), so one
    /// socket read routinely delivers a COMPLETE ClientHello PLUS trailing
    /// bytes that push the window over `max_bytes`. The cap is a rejection
    /// criterion ONLY while the hello is still incomplete -- a complete
    /// hello must route regardless of total window length, and the trailing
    /// bytes stay in the accumulator for byte-for-byte replay.
    #[test]
    fn complete_hello_with_trailing_bytes_over_cap_routes() {
        let mut routes = TrieNode::root();
        routes.domain_insert(
            b"example.com".to_vec(),
            vec![(AlpnMatcher::Any, "cluster-a".to_owned())],
        );
        let wire = hello("example.com", &[b"h2"]);
        let mut over = wire.clone();
        over.extend_from_slice(&[0xAB; 64]); // post-hello handshake bytes

        let mut small_cfg = cfg(&routes);
        small_cfg.max_bytes = wire.len() + 16; // cap between hello and total
        assert!(over.len() > small_cfg.max_bytes, "test setup: total > cap");

        let mut core = SniPrereadCore::new();
        match feed(&mut core, &small_cfg, &over) {
            Output::Routed {
                cluster,
                content_offset,
                sni,
                ..
            } => {
                assert_eq!(cluster, "cluster-a");
                assert_eq!(content_offset, 0);
                assert_eq!(sni, "example.com");
            }
            other => panic!("a complete hello must route despite trailing bytes, got {other:?}"),
        }
    }

    /// The PROXY-header-incomplete branch must respect the cap too: a
    /// partial PROXY-v2 header whose window has already reached `max_bytes`
    /// can never complete -- `TooLarge`, not an eternal `NeedMore`.
    #[test]
    fn proxy_incomplete_at_cap_is_too_large() {
        let routes = TrieNode::root();
        let mut proxy_cfg = cfg(&routes);
        proxy_cfg.inbound_proxy = true;
        proxy_cfg.max_bytes = 10;

        let src = SocketAddr::new(IpAddr::V4(Ipv4Addr::new(203, 0, 113, 7)), 51234);
        let dst = SocketAddr::new(IpAddr::V4(Ipv4Addr::new(198, 51, 100, 9)), 443);
        let proxy_header = HeaderV2::new(Command::Proxy, src, dst).into_bytes();

        let mut core = SniPrereadCore::new();
        let out = feed(&mut core, &proxy_cfg, &proxy_header[..10]);
        assert_eq!(out, Output::Reject(RejectReason::TooLarge));
    }

    #[test]
    fn empty_buffer_needs_more_and_sets_deadline() {
        let routes = TrieNode::root();
        let cfg = cfg(&routes);
        let mut core = SniPrereadCore::new();
        match feed(&mut core, &cfg, &[]) {
            Output::NeedMore { .. } => {}
            other => panic!("expected NeedMore on empty input, got {other:?}"),
        }
    }

    #[test]
    fn timeout_while_undecided_is_fragmented() {
        let routes = TrieNode::root();
        let cfg = cfg(&routes);
        let mut core = SniPrereadCore::new();
        // Partial ClientHello: never enough to decide.
        let wire = hello_no_alpn("example.com");
        let _ = feed(&mut core, &cfg, &wire[..wire.len() - 1]);
        let out = core.handle_input(
            &cfg,
            Input::Timeout {
                now: Instant::now(),
            },
        );
        assert_eq!(out, Output::Reject(RejectReason::Fragmented));
    }

    #[test]
    fn front_closed_before_decision_is_front_closed() {
        let routes = TrieNode::root();
        let cfg = cfg(&routes);
        let mut core = SniPrereadCore::new();
        let out = core.handle_input(&cfg, Input::FrontClosed);
        assert_eq!(out, Output::Reject(RejectReason::FrontClosed));
    }

    #[test]
    fn decided_latch_replays_the_same_terminal_forever() {
        let mut routes = TrieNode::root();
        routes.domain_insert(
            b"example.com".to_vec(),
            vec![(AlpnMatcher::Any, "cluster-a".to_owned())],
        );
        let cfg = cfg(&routes);
        let mut core = SniPrereadCore::new();
        let wire = hello_no_alpn("example.com");
        let first = feed(&mut core, &cfg, &wire);
        // Feed again (even with clearly different bytes) -- must replay.
        let second = feed(&mut core, &cfg, &[0xff; 3]);
        assert_eq!(first, second);
        let third = core.handle_input(
            &cfg,
            Input::Timeout {
                now: Instant::now(),
            },
        );
        assert_eq!(first, third);
    }

    #[test]
    fn not_tls_is_reachable_through_the_core() {
        let routes = TrieNode::root();
        let cfg = cfg(&routes);
        let mut core = SniPrereadCore::new();
        let out = feed(&mut core, &cfg, &[0x16 + 1, 0x03, 0x03, 0x00, 0x00]);
        assert_eq!(out, Output::Reject(RejectReason::NotTls));
    }

    #[test]
    fn no_sni_is_reachable() {
        let mut routes = TrieNode::root();
        routes.domain_insert(
            b"example.com".to_vec(),
            vec![(AlpnMatcher::Any, "cluster-a".to_owned())],
        );
        let cfg = cfg(&routes);
        let mut core = SniPrereadCore::new();
        let wire = parser::build_client_hello_wire(&[]);
        assert_eq!(
            feed(&mut core, &cfg, &wire),
            Output::Reject(RejectReason::NoSni)
        );
    }

    #[test]
    fn ech_outer_absent_is_reachable() {
        let mut routes = TrieNode::root();
        routes.domain_insert(
            b"example.com".to_vec(),
            vec![(AlpnMatcher::Any, "cluster-a".to_owned())],
        );
        let cfg = cfg(&routes);
        let mut core = SniPrereadCore::new();
        let wire = parser::build_client_hello_wire(&[parser::encode_extension(
            0xfe0d,
            &[0x00, 0x01, 0x02],
        )]);
        assert_eq!(
            feed(&mut core, &cfg, &wire),
            Output::Reject(RejectReason::EchOuterAbsent)
        );
    }

    #[test]
    fn sni_unmatched_is_reachable() {
        let mut routes = TrieNode::root();
        routes.domain_insert(
            b"example.com".to_vec(),
            vec![(AlpnMatcher::Any, "cluster-a".to_owned())],
        );
        let cfg = cfg(&routes);
        let mut core = SniPrereadCore::new();
        let wire = hello_no_alpn("unknown.example.net");
        assert_eq!(
            feed(&mut core, &cfg, &wire),
            Output::Reject(RejectReason::SniUnmatched)
        );
    }

    #[test]
    fn alpn_unmatched_is_reachable() {
        let mut routes = TrieNode::root();
        routes.domain_insert(
            b"example.com".to_vec(),
            vec![(one_of(&[b"h2"]), "cluster-a".to_owned())],
        );
        let cfg = cfg(&routes);
        let mut core = SniPrereadCore::new();
        let wire = hello("example.com", &[b"http/1.1"]);
        assert_eq!(
            feed(&mut core, &cfg, &wire),
            Output::Reject(RejectReason::AlpnUnmatched)
        );
    }

    #[test]
    fn proxy_header_invalid_is_reachable() {
        let routes = TrieNode::root();
        let mut proxy_cfg = cfg(&routes);
        proxy_cfg.inbound_proxy = true;
        let mut core = SniPrereadCore::new();
        // 16 garbage bytes: long enough not to be `Incomplete` against the
        // 12-byte signature tag, but not a valid PROXY-v2 header.
        let out = feed(&mut core, &proxy_cfg, &[0xAAu8; 16]);
        assert_eq!(out, Output::Reject(RejectReason::ProxyHeaderInvalid));
    }

    #[test]
    fn malformed_record_is_reachable() {
        let routes = TrieNode::root();
        let cfg = cfg(&routes);
        let mut core = SniPrereadCore::new();
        // ContentType = handshake, declared length far above the 2^14 cap.
        let record = [0x16, 0x03, 0x03, 0xFF, 0xFF];
        assert_eq!(
            feed(&mut core, &cfg, &record),
            Output::Reject(RejectReason::MalformedRecord)
        );
    }

    #[test]
    fn malformed_handshake_is_reachable() {
        let routes = TrieNode::root();
        let cfg = cfg(&routes);
        let mut core = SniPrereadCore::new();
        let hs = parser::wrap_handshake(&[]); // msg_type=1, length=0 body -- fine
        let mut bad_hs = hs;
        bad_hs[0] = 2; // ServerHello, not ClientHello
        let record = parser::wrap_record(22, &bad_hs);
        assert_eq!(
            feed(&mut core, &cfg, &record),
            Output::Reject(RejectReason::MalformedHandshake)
        );
    }

    // ---- fragmentation through the core ------------------------------------

    #[test]
    fn drip_feed_needs_more_then_routes() {
        let mut routes = TrieNode::root();
        routes.domain_insert(
            b"example.com".to_vec(),
            vec![(AlpnMatcher::Any, "cluster-a".to_owned())],
        );
        let cfg = cfg(&routes);
        let wire = hello("example.com", &[b"h2"]);
        let mut core = SniPrereadCore::new();
        for i in 0..wire.len() {
            match feed(&mut core, &cfg, &wire[..i]) {
                Output::NeedMore { .. } => {}
                other => panic!("prefix {i} of {} must NeedMore, got {other:?}", wire.len()),
            }
        }
        match feed(&mut core, &cfg, &wire) {
            Output::Routed { cluster, sni, .. } => {
                assert_eq!(cluster, "cluster-a");
                assert_eq!(sni, "example.com");
            }
            other => panic!("expected Routed at full length, got {other:?}"),
        }
    }

    #[test]
    fn multi_record_client_hello_routes_through_the_core() {
        let mut routes = TrieNode::root();
        routes.domain_insert(
            b"split.example.com".to_vec(),
            vec![(AlpnMatcher::Any, "cluster-split".to_owned())],
        );
        let cfg = cfg(&routes);
        let wire = hello("split.example.com", &[b"h2"]);
        let split = parser::split_into_records(&wire, 4);
        let mut core = SniPrereadCore::new();
        match feed(&mut core, &cfg, &split) {
            Output::Routed { cluster, .. } => assert_eq!(cluster, "cluster-split"),
            other => panic!("expected Routed for a multi-record ClientHello, got {other:?}"),
        }
    }

    // ---- PROXY-v2 prefixed feeds --------------------------------------------

    #[test]
    fn proxy_v2_prefix_yields_content_offset_and_source() {
        let mut routes = TrieNode::root();
        routes.domain_insert(
            b"example.com".to_vec(),
            vec![(AlpnMatcher::Any, "cluster-a".to_owned())],
        );
        let mut proxy_cfg = cfg(&routes);
        proxy_cfg.inbound_proxy = true;

        let src = SocketAddr::new(IpAddr::V4(Ipv4Addr::new(203, 0, 113, 7)), 51234);
        let dst = SocketAddr::new(IpAddr::V4(Ipv4Addr::new(198, 51, 100, 9)), 443);
        let proxy_header = HeaderV2::new(Command::Proxy, src, dst).into_bytes();
        // The 28-byte IPv4 v2 header (16-byte fixed prefix + 12-byte IPv4
        // address block) precedes the ClientHello on the wire.
        assert_eq!(proxy_header.len(), 28);

        let mut wire = proxy_header.clone();
        wire.extend_from_slice(&hello_no_alpn("example.com"));

        let mut core = SniPrereadCore::new();
        match feed(&mut core, &proxy_cfg, &wire) {
            Output::Routed {
                cluster,
                content_offset,
                proxy_source,
                ..
            } => {
                assert_eq!(cluster, "cluster-a");
                assert_eq!(content_offset, proxy_header.len());
                assert_eq!(proxy_source, Some(src));
            }
            other => panic!("expected Routed behind a PROXY-v2 header, got {other:?}"),
        }
    }

    #[test]
    fn proxy_v2_prefix_drip_feed_needs_more_until_complete() {
        let mut routes = TrieNode::root();
        routes.domain_insert(
            b"example.com".to_vec(),
            vec![(AlpnMatcher::Any, "cluster-a".to_owned())],
        );
        let mut proxy_cfg = cfg(&routes);
        proxy_cfg.inbound_proxy = true;

        let src = SocketAddr::new(IpAddr::V4(Ipv4Addr::new(203, 0, 113, 7)), 51234);
        let dst = SocketAddr::new(IpAddr::V4(Ipv4Addr::new(198, 51, 100, 9)), 443);
        let proxy_header = HeaderV2::new(Command::Proxy, src, dst).into_bytes();
        let mut wire = proxy_header.clone();
        wire.extend_from_slice(&hello_no_alpn("example.com"));

        let mut core = SniPrereadCore::new();
        for i in 0..proxy_header.len() {
            match feed(&mut core, &proxy_cfg, &wire[..i]) {
                Output::NeedMore { .. } => {}
                other => panic!("PROXY-header prefix {i} must NeedMore, got {other:?}"),
            }
        }
    }

    // ---- route semantics: SNI precedence + ALPN resolution ------------------

    #[test]
    fn exact_sni_beats_wildcard_before_alpn() {
        let mut routes = TrieNode::root();
        routes.domain_insert(
            b"*.example.com".to_vec(),
            vec![(AlpnMatcher::Any, "wildcard-cluster".to_owned())],
        );
        routes.domain_insert(
            b"a.example.com".to_vec(),
            vec![(one_of(&[b"h2"]), "exact-cluster".to_owned())],
        );
        let cfg = cfg(&routes);

        // The exact route wins even though its ALPN entry, taken alone,
        // would reject an `http/1.1`-only client -- exact-beats-wildcard is
        // resolved by the trie BEFORE ALPN is even considered.
        let wire = hello("a.example.com", &[b"http/1.1"]);
        let mut core = SniPrereadCore::new();
        assert_eq!(
            feed(&mut core, &cfg, &wire),
            Output::Reject(RejectReason::AlpnUnmatched)
        );
    }

    #[test]
    fn wildcard_matches_one_level_not_two_not_apex() {
        let mut routes = TrieNode::root();
        routes.domain_insert(
            b"*.example.com".to_vec(),
            vec![(AlpnMatcher::Any, "wildcard-cluster".to_owned())],
        );
        let cfg = cfg(&routes);

        let mut core = SniPrereadCore::new();
        match feed(&mut core, &cfg, &hello_no_alpn("a.example.com")) {
            Output::Routed {
                cluster,
                sni,
                matched_sni_pattern,
                matched_alpn,
                ..
            } => {
                assert_eq!(cluster, "wildcard-cluster");
                // The matched-route identity is the trie KEY (the
                // configured wildcard pattern), NOT the client's concrete
                // SNI -- downstream tags keying depends on this.
                assert_eq!(sni, "a.example.com");
                assert_eq!(matched_sni_pattern, "*.example.com");
                assert_eq!(matched_alpn, AlpnMatcher::Any);
            }
            other => panic!("*.example.com must match a.example.com, got {other:?}"),
        }

        let mut core = SniPrereadCore::new();
        assert_eq!(
            feed(&mut core, &cfg, &hello_no_alpn("a.b.example.com")),
            Output::Reject(RejectReason::SniUnmatched),
            "*.example.com must NOT match a.b.example.com"
        );

        let mut core = SniPrereadCore::new();
        assert_eq!(
            feed(&mut core, &cfg, &hello_no_alpn("example.com")),
            Output::Reject(RejectReason::SniUnmatched),
            "*.example.com must NOT match the apex example.com"
        );
    }

    #[test]
    fn alpn_client_preference_order_wins() {
        let mut routes = TrieNode::root();
        routes.domain_insert(
            b"example.com".to_vec(),
            vec![
                (one_of(&[b"http/1.1"]), "cluster-http11".to_owned()),
                (one_of(&[b"h2"]), "cluster-h2".to_owned()),
            ],
        );
        let cfg = cfg(&routes);

        // Client prefers h2 first, even though the http/1.1 entry is FIRST
        // in the route table -- client order wins.
        let mut core = SniPrereadCore::new();
        match feed(
            &mut core,
            &cfg,
            &hello("example.com", &[b"h2", b"http/1.1"]),
        ) {
            Output::Routed { cluster, .. } => assert_eq!(cluster, "cluster-h2"),
            other => panic!("expected the client's first preference, got {other:?}"),
        }

        let mut core = SniPrereadCore::new();
        match feed(
            &mut core,
            &cfg,
            &hello("example.com", &[b"http/1.1", b"h2"]),
        ) {
            Output::Routed { cluster, .. } => assert_eq!(cluster, "cluster-http11"),
            other => panic!("expected the client's first preference, got {other:?}"),
        }
    }

    #[test]
    fn alpn_any_catch_all_and_no_alpn_client_matches_any_only() {
        let mut routes = TrieNode::root();
        routes.domain_insert(
            b"example.com".to_vec(),
            vec![
                (one_of(&[b"h2"]), "cluster-h2".to_owned()),
                (AlpnMatcher::Any, "cluster-default".to_owned()),
            ],
        );
        let cfg = cfg(&routes);

        // Offers something nobody claims via OneOf -> falls through to Any.
        let mut core = SniPrereadCore::new();
        match feed(&mut core, &cfg, &hello("example.com", &[b"spdy/1"])) {
            Output::Routed { cluster, .. } => assert_eq!(cluster, "cluster-default"),
            other => panic!("expected the Any catch-all, got {other:?}"),
        }

        // A client offering NO alpn extension at all matches only Any.
        let mut core = SniPrereadCore::new();
        match feed(&mut core, &cfg, &hello_no_alpn("example.com")) {
            Output::Routed { cluster, alpn, .. } => {
                assert_eq!(cluster, "cluster-default");
                assert!(alpn.is_empty());
            }
            other => panic!("expected the Any catch-all for a no-ALPN client, got {other:?}"),
        }
    }

    #[test]
    fn no_any_and_no_matching_one_of_is_alpn_unmatched() {
        let mut routes = TrieNode::root();
        routes.domain_insert(
            b"example.com".to_vec(),
            vec![(one_of(&[b"h2"]), "cluster-h2".to_owned())],
        );
        let cfg = cfg(&routes);
        let mut core = SniPrereadCore::new();
        assert_eq!(
            feed(&mut core, &cfg, &hello_no_alpn("example.com")),
            Output::Reject(RejectReason::AlpnUnmatched),
            "a no-ALPN client with no Any entry must be unmatched, not silently routed"
        );
    }
}
