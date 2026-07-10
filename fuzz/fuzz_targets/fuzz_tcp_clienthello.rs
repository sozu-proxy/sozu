#![no_main]
//! Fuzz target for the sans-io TCP passthrough SNI-preread core (issue #1279).
//!
//! Drives [`SniPrereadCore::handle_input`] through an arbitrary sequence of
//! growing-window byte feeds, deadline timeouts, and front-close events,
//! against a small route table derived deterministically from the fuzz
//! input -- all through the PUBLIC API of
//! `sozu_lib::protocol::tcp_preread` (plus the already-public
//! `sozu_lib::router::pattern_trie::TrieNode` it reuses for routing).
//!
//! The core is pure sans-io: no socket, no `Instant::now()` on the datapath
//! beyond the one seed captured here, no `rand`. To stay deterministic this
//! target injects a monotonic clock -- a single base `Instant` captured
//! once, advanced only by deltas parsed out of the input -- so the same
//! input always produces the same run.
//!
//! Grammar (big-endian `Reader` over the fuzz input, mirrors
//! `fuzz_udp_flow.rs`):
//! 1. A route table: 1-4 entries, each an SNI key drawn from a small fixed
//!    pool (`SNI_POOL`, includes exact hosts and `*.` wildcards) mapped to
//!    an [`AlpnMatcher`] (`Any`, or a 1-2-protocol `OneOf` drawn from
//!    `ALPN_POOL`) and a synthesized cluster id, inserted via
//!    [`TrieNode::domain_insert`].
//! 2. A [`PrereadConfig`]: `inbound_proxy` flag, `max_bytes` in `64..=16384`,
//!    a fixed 3-second timeout, and an `accept_wildcard` flag.
//! 3. A bounded (<= 4096 iterations) step loop: each step either grows the
//!    accumulated window by a length-prefixed chunk taken DIRECTLY from the
//!    remaining fuzz bytes (so libFuzzer's coverage-guided mutation shapes
//!    the wire bytes the core actually parses) and feeds
//!    `Input::Bytes { buf: &window, now }`, advances the injected clock by a
//!    parsed delta, or injects `Input::Timeout` / `Input::FrontClosed`.
//!
//! Invariants asserted beyond "never panic" (see `check_output`):
//! - the fed window is byte-identical before and after every call;
//! - once a terminal `Output` (`Routed` / `Reject`) is latched, every later
//!   call -- of any kind -- returns that SAME terminal, and `NeedMore` can
//!   never appear again;
//! - `Routed::content_offset` never exceeds the window length it was
//!   derived from;
//! - a `NeedMore::deadline`, once observed, never decreases across calls.
//!
//! Corpus + run instructions live in `fuzz/README.md`.

use std::{
    collections::BTreeSet,
    time::{Duration, Instant},
};

use libfuzzer_sys::fuzz_target;
use sozu_lib::{
    protocol::tcp_preread::{AlpnMatcher, Input, Output, PrereadConfig, SniPrereadCore},
    router::pattern_trie::TrieNode,
};

/// `sozu_command::state::ClusterId` is a plain `String` alias; re-declaring
/// it locally avoids pulling `sozu-command` into the fuzz crate's dependency
/// graph just for a type alias.
type ClusterId = String;

/// Fixed SNI key pool: a mix of exact hosts and `*.` wildcards so both the
/// exact-match and wildcard-match trie paths are reachable.
const SNI_POOL: [&[u8]; 4] = [
    b"a.example.com",
    b"*.example.com",
    b"b.example.net",
    b"*.wild.example.org",
];

/// Fixed ALPN protocol-id pool for [`AlpnMatcher::OneOf`] sets.
const ALPN_POOL: [&[u8]; 4] = [b"h2", b"http/1.1", b"h3", b"foo"];

/// A tiny big-endian byte reader over the fuzz input, mirroring
/// `fuzz_udp_flow.rs::Reader`. Every getter returns a default (0) when the
/// input is exhausted, so the grammar degrades gracefully on short inputs.
struct Reader<'a> {
    data: &'a [u8],
    pos: usize,
}

impl<'a> Reader<'a> {
    fn new(data: &'a [u8]) -> Self {
        Reader { data, pos: 0 }
    }

    fn is_empty(&self) -> bool {
        self.pos >= self.data.len()
    }

    fn u8(&mut self) -> u8 {
        let b = self.data.get(self.pos).copied().unwrap_or(0);
        self.pos += 1;
        b
    }

    fn u16(&mut self) -> u16 {
        u16::from_be_bytes([self.u8(), self.u8()])
    }

    /// Read a length-prefixed slice (1-byte length, capped at the remaining
    /// input) taken DIRECTLY from the fuzz bytes -- not synthesized -- so
    /// the wire content fed to the core is whatever libFuzzer's
    /// coverage-guided mutation puts there.
    fn chunk(&mut self) -> &'a [u8] {
        let len = self.u8() as usize;
        let start = self.pos.min(self.data.len());
        let end = (start + len).min(self.data.len());
        self.pos = end;
        &self.data[start..end]
    }
}

/// Build a small route table: 1-4 entries keyed from `SNI_POOL`, each mapped
/// to a one-element `(AlpnMatcher, ClusterId)` route entry.
fn build_routes(r: &mut Reader) -> TrieNode<Vec<(AlpnMatcher, ClusterId)>> {
    let mut routes: TrieNode<Vec<(AlpnMatcher, ClusterId)>> = TrieNode::root();
    let count = 1 + (r.u8() as usize % 4);
    for i in 0..count {
        let key = SNI_POOL[r.u8() as usize % SNI_POOL.len()].to_vec();
        let matcher = if r.u8() & 1 == 0 {
            AlpnMatcher::Any
        } else {
            let n = 1 + (r.u8() as usize % 2);
            let mut set = BTreeSet::new();
            for _ in 0..n {
                set.insert(ALPN_POOL[r.u8() as usize % ALPN_POOL.len()].to_vec());
            }
            AlpnMatcher::OneOf(set)
        };
        routes.domain_insert(key, vec![(matcher, format!("cluster-{i}"))]);
    }
    routes
}

/// Validate one `Output` against the running invariants and update the
/// tracked state (`last_deadline`, `terminal`). Panics (via `assert!`) on
/// violation -- that panic IS the fuzz finding.
fn check_output(
    out: &Output,
    window_len: usize,
    last_deadline: &mut Option<Instant>,
    terminal: &mut Option<Output>,
) {
    match out {
        Output::NeedMore { deadline } => {
            assert!(
                terminal.is_none(),
                "NeedMore returned after a terminal verdict was already latched"
            );
            if let Some(prev) = *last_deadline {
                assert!(
                    *deadline >= prev,
                    "NeedMore deadline decreased across calls: {deadline:?} < {prev:?}"
                );
            }
            *last_deadline = Some(*deadline);
        }
        Output::Routed { content_offset, .. } => {
            assert!(
                *content_offset <= window_len,
                "Routed.content_offset {content_offset} exceeded window length {window_len}"
            );
            latch_terminal(out, terminal);
        }
        Output::Reject(_) => latch_terminal(out, terminal),
    }
}

/// Latch the first terminal `Output` seen and assert every later terminal
/// replays it identically -- the monotonic decided-latch contract
/// documented on [`SniPrereadCore`].
fn latch_terminal(out: &Output, terminal: &mut Option<Output>) {
    match terminal {
        Some(prev) => assert_eq!(
            prev, out,
            "terminal verdict changed across calls -- decided latch violated"
        ),
        None => *terminal = Some(out.clone()),
    }
}

fuzz_target!(|data: &[u8]| {
    let mut r = Reader::new(data);

    let routes = build_routes(&mut r);

    let inbound_proxy = r.u8() & 1 != 0;
    let max_bytes = 64 + (r.u16() as usize % (16384 - 64 + 1));
    let accept_wildcard = r.u8() & 1 != 0;

    let cfg = PrereadConfig {
        routes: &routes,
        inbound_proxy,
        max_bytes,
        timeout: Duration::from_secs(3),
        accept_wildcard,
    };

    let mut core = SniPrereadCore::new();
    let base = Instant::now();
    let mut now = base;
    // The FULL accumulated window from wire offset 0 -- re-fed on every
    // `Input::Bytes`, never a delta (mirrors the shell contract documented
    // on `Input::Bytes`).
    let mut window: Vec<u8> = Vec::new();
    let mut terminal: Option<Output> = None;
    let mut last_deadline: Option<Instant> = None;

    let mut steps = 0usize;
    while !r.is_empty() && steps < 4096 {
        steps += 1;
        match r.u8() % 8 {
            // Grow the window and feed it (5/8 of the step space: the
            // common case).
            0..=4 => {
                let chunk = r.chunk();
                window.extend_from_slice(chunk);
                let before = window.clone();
                let out = core.handle_input(&cfg, Input::Bytes { buf: &window, now });
                assert_eq!(window, before, "the core must never mutate the fed window");
                check_output(&out, window.len(), &mut last_deadline, &mut terminal);
            }
            // Advance the injected clock without feeding bytes (never
            // rewound -- monotonic).
            5 => {
                let delta_ms = (r.u16() as u64) % 5_000;
                now += Duration::from_millis(delta_ms);
            }
            // The preread deadline fired.
            6 => {
                let out = core.handle_input(&cfg, Input::Timeout { now });
                check_output(&out, window.len(), &mut last_deadline, &mut terminal);
            }
            // The frontend socket closed.
            _ => {
                let out = core.handle_input(&cfg, Input::FrontClosed);
                check_output(&out, window.len(), &mut last_deadline, &mut terminal);
            }
        }
    }
});
