// Gated on `--cfg tokio_unstable`: moonpool-sim seeds tokio's runtime RNG via the
// unstable `RngSeed` API. Without the flag this whole test crate compiles to an
// empty (0-test) binary and pulls no moonpool/tokio deps (see Cargo.toml), so a
// plain `cargo test --workspace` stays free of tokio_unstable. Run the real sweep
// with `RUSTFLAGS="--cfg tokio_unstable" cargo test -p sozu-sim --test tcp_preread_sim`.
#![cfg(tokio_unstable)]
//! Deterministic simulation of the sans-io TCP passthrough SNI-preread core
//! (`sozu_lib::protocol::tcp_preread::SniPrereadCore`, issue #1279), driven by
//! the [moonpool-sim] engine on the same pattern as [`udp_simulation.rs`] (see
//! `doc/udp_simulation.md` for the general recipe).
//!
//! Unlike [`UdpManager`], which is a long-lived flow table driven step by
//! step, [`SniPrereadCore`] is a small, near-stateless PER-CONNECTION state
//! machine: it is fed the full accumulated preread window on every
//! [`Input::Bytes`] and latches exactly one terminal [`Output`] (`Routed` or
//! `Reject`) for its whole lifetime. So instead of one manager stepped
//! through many actions, this harness simulates MANY independent
//! connections per seed -- each with its own fresh [`SniPrereadCore`], its
//! own randomly-generated ClientHello-shaped wire, and its own fragmentation
//! schedule -- and folds every connection's outcome into a shared coverage
//! tally plus a per-seed fingerprint.
//!
//! # Scenario generator
//!
//! [`generate_scenario`] draws a weighted-random [`Scenario`] from a mix of
//! DIRECTED generators (one per required [`RejectReason`] variant plus
//! `Routed`, multi-record hellos, and forced single-byte-drip delivery -- so
//! every class is reachable BY CONSTRUCTION, not by hoping randomness finds
//! it) and a chaos generator (`gen_random_mutation_chaos`) that fuzzes
//! arbitrary bytes with an unconstrained expected outcome. A low-probability
//! `buggify_with_prob!` bit-flip additionally perturbs otherwise-directed
//! wires (clearing their expectation, never their invariants).
//!
//! ClientHello wires are built by a small hand-rolled encoder in this file
//! (record layer + handshake + `server_name`/`alpn`/GREASE/ECH extensions):
//! the core's own wire builders in `tcp_preread/{mod,parser}.rs` are
//! `#[cfg(test)]`-private to that module and are not linked into this
//! integration-test binary.
//!
//! # Invariants checked per connection
//!
//! - **Byte-window untouched**: an FNV-1a hash of the fed window is compared
//!   before/after every `handle_input` call (structurally guaranteed by
//!   `buf: &[u8]` being immutable, but checked explicitly as a harness-level
//!   regression guard against a future `unsafe` slip).
//! - **Deadline armed once, never rewound**: every `NeedMore { deadline }`
//!   seen for one connection must carry the SAME deadline as the first one
//!   (the core's own `debug_assert!`s check this internally too; this is the
//!   harness's external, black-box confirmation of the same contract).
//! - **Terminal-at-most-once / latched replay identical**: once a connection
//!   reaches a terminal `Output`, one more redundant `handle_input` call (a
//!   randomly chosen `Bytes` / `Timeout` / `FrontClosed`) must replay the
//!   EXACT SAME terminal output.
//! - **Route determinism**: a second, fresh `SniPrereadCore` fed the exact
//!   same wire (one shot) against the same route table must reach the same
//!   terminal -- UNLESS the original connection needed an out-of-band
//!   `Timeout`/`FrontClosed` signal to decide (a RUNTIME fact, not a static
//!   per-scenario-class one -- both the directed `fragmented_timeout` /
//!   `front_closed_now` generators and an unlucky `random_mutation_chaos`
//!   wire can land here), in which case the check instead confirms the wire
//!   genuinely stays `NeedMore` on its own.
//!
//! # Hard coverage gate
//!
//! The default sweep asserts, after merging every seed's tally, that
//! `accepted` and EVERY [`RejectReason`] counter, plus `fragmented_delivery`,
//! `multi_record`, and `complete_over_cap` (a COMPLETE hello with trailing
//! bytes past `max_bytes` that still routes -- the regression class for the
//! `on_bytes` reorder that made `TooLarge` conditional on genuine
//! incompleteness), are non-zero -- see [`CoverageTally::assert_full_coverage`].
//! A sweep that never exercises one of these classes fails loudly rather than
//! silently under-covering the core.
//!
//! # Replay / sweep ergonomics
//!
//! - `SOZU_TCP_PREREAD_SIM_SEED=<u64|0xhex>` -- replay that ONE seed verbosely.
//! - `SOZU_TCP_PREREAD_SIM_SEEDS=<n>` -- sweep `n` seeds (default 256).
//! - `SOZU_TCP_PREREAD_SIM_STEPS=<n>` -- connections simulated PER SEED
//!   (default 48; kept modest because a connection's cost is dominated by its
//!   fragmentation schedule, not by a fixed per-step cost like the UDP sim).
//!
//! [moonpool-sim]: https://crates.io/crates/moonpool-sim
//! [`udp_simulation.rs`]: ../../sim/tests/udp_simulation.rs
//! [`UdpManager`]: sozu_lib::protocol::udp::UdpManager

use std::{
    collections::BTreeSet,
    net::{IpAddr, Ipv4Addr, SocketAddr},
    sync::{Arc, Mutex},
    time::{Duration, Instant},
};

use async_trait::async_trait;
use moonpool_sim::{
    RandomProvider, SimContext, SimulationBuilder, SimulationReport, SimulationResult,
    TimeProvider, Workload, buggify_with_prob, current_sim_seed,
};
use sozu_lib::{
    protocol::{
        proxy_protocol::header::{Command, HeaderV2},
        tcp_preread::{AlpnMatcher, Input, Output, PrereadConfig, RejectReason, SniPrereadCore},
    },
    router::pattern_trie::TrieNode,
};

// --------------------------------------------------------------------------
// Hand-rolled ClientHello / TLS-record wire encoder.
//
// This deliberately duplicates the shape of the (private, #[cfg(test)]-only)
// builders in `lib/src/protocol/tcp_preread/parser.rs` -- they are not
// visible from this external integration-test crate, and the module doc
// asks for "your own small builder" rather than reaching for them.
// --------------------------------------------------------------------------

const TLS_CONTENT_TYPE_HANDSHAKE: u8 = 22;
const TLS_HANDSHAKE_TYPE_CLIENT_HELLO: u8 = 1;
const MAX_TLS_RECORD_LEN: usize = 16384;
const EXT_SERVER_NAME: u16 = 0x0000;
const EXT_ALPN: u16 = 0x0010;
const EXT_ECH: u16 = 0xfe0d;

fn encode_extension(ext_type: u16, data: &[u8]) -> Vec<u8> {
    let mut out = Vec::with_capacity(4 + data.len());
    out.extend_from_slice(&ext_type.to_be_bytes());
    out.extend_from_slice(&(data.len() as u16).to_be_bytes());
    out.extend_from_slice(data);
    out
}

fn encode_sni_extension(host: &str) -> Vec<u8> {
    let mut name_list = vec![0u8]; // name_type = host_name
    name_list.extend_from_slice(&(host.len() as u16).to_be_bytes());
    name_list.extend_from_slice(host.as_bytes());
    let mut data = Vec::new();
    data.extend_from_slice(&(name_list.len() as u16).to_be_bytes());
    data.extend_from_slice(&name_list);
    encode_extension(EXT_SERVER_NAME, &data)
}

fn encode_alpn_extension(protocols: &[&[u8]]) -> Vec<u8> {
    let mut list = Vec::new();
    for p in protocols {
        list.push(p.len() as u8);
        list.extend_from_slice(p);
    }
    let mut data = Vec::new();
    data.extend_from_slice(&(list.len() as u16).to_be_bytes());
    data.extend_from_slice(&list);
    encode_extension(EXT_ALPN, &data)
}

/// RFC 8701 GREASE extension value (`0x?A?A` pattern), randomized per call so
/// the parser proves it skips ANY GREASE codepoint by length, not just one.
fn encode_grease_extension(ctx: &SimContext) -> Vec<u8> {
    let nibble: u16 = ctx.random().random_range(0..16u16);
    let ext_type = (nibble << 12) | 0x0A0A;
    encode_extension(ext_type, &[0x00])
}

fn build_client_hello_body(extra_extensions: &[Vec<u8>]) -> Vec<u8> {
    let mut body = Vec::new();
    body.extend_from_slice(&[0x03, 0x03]); // legacy_version {3, 3}
    body.extend_from_slice(&[0u8; 32]); // random
    body.push(0); // session_id: empty
    body.extend_from_slice(&[0x00, 0x02, 0x13, 0x01]); // cipher_suites: TLS_AES_128_GCM_SHA256
    body.push(1); // compression_methods length = 1
    body.push(0); // compression_method: null

    let mut ext_block = Vec::new();
    for ext in extra_extensions {
        ext_block.extend_from_slice(ext);
    }
    body.extend_from_slice(&(ext_block.len() as u16).to_be_bytes());
    body.extend_from_slice(&ext_block);
    body
}

fn wrap_handshake_with_type(msg_type: u8, body: &[u8]) -> Vec<u8> {
    let mut hs = Vec::with_capacity(4 + body.len());
    hs.push(msg_type);
    let len = body.len() as u32;
    hs.extend_from_slice(&len.to_be_bytes()[1..4]);
    hs.extend_from_slice(body);
    hs
}

fn wrap_record(content_type: u8, payload: &[u8]) -> Vec<u8> {
    let mut rec = Vec::with_capacity(5 + payload.len());
    rec.push(content_type);
    rec.extend_from_slice(&[0x03, 0x03]); // legacy record version
    rec.extend_from_slice(&(payload.len() as u16).to_be_bytes());
    rec.extend_from_slice(payload);
    rec
}

/// Build a single-record wire ClientHello:
/// `wrap_record(22, wrap_handshake(TYPE_CLIENT_HELLO, build_client_hello_body(...)))`.
fn build_client_hello_wire(extra_extensions: &[Vec<u8>]) -> Vec<u8> {
    wrap_record(
        TLS_CONTENT_TYPE_HANDSHAKE,
        &wrap_handshake_with_type(
            TLS_HANDSHAKE_TYPE_CLIENT_HELLO,
            &build_client_hello_body(extra_extensions),
        ),
    )
}

/// Split a wire ClientHello's SINGLE record back into `chunk_count` records
/// that together reassemble to the same handshake bytes -- exercising the
/// multi-record reassembly path. `wire` must be exactly one TLS record (as
/// produced by [`build_client_hello_wire`]).
fn split_into_records(wire: &[u8], chunk_count: usize) -> Vec<u8> {
    let chunk_count = chunk_count.max(1);
    let payload = &wire[5..];
    let chunk_len = payload.len().div_ceil(chunk_count).max(1);
    let mut out = Vec::new();
    for chunk in payload.chunks(chunk_len) {
        out.extend_from_slice(&wrap_record(TLS_CONTENT_TYPE_HANDSHAKE, chunk));
    }
    out
}

fn hello(sni: &str, alpn: &[&[u8]]) -> Vec<u8> {
    build_client_hello_wire(&[encode_sni_extension(sni), encode_alpn_extension(alpn)])
}

fn hello_no_alpn(sni: &str) -> Vec<u8> {
    build_client_hello_wire(&[encode_sni_extension(sni)])
}

fn one_of(protocols: &[&[u8]]) -> AlpnMatcher {
    AlpnMatcher::OneOf(
        protocols
            .iter()
            .map(|p| p.to_vec())
            .collect::<BTreeSet<_>>(),
    )
}

fn routed(
    cluster: &str,
    sni: &str,
    alpn: &[&[u8]],
    content_offset: usize,
    proxy_source: Option<SocketAddr>,
) -> Output {
    Output::Routed {
        cluster: cluster.to_owned(),
        content_offset,
        proxy_source,
        sni: sni.to_owned(),
        alpn: alpn.iter().map(|p| p.to_vec()).collect(),
    }
}

// --------------------------------------------------------------------------
// Fixed route table: exact hosts, one wildcard, and every ALPN matcher shape
// (Any-only, OneOf-only with no fallback, and OneOf+Any mixed).
// --------------------------------------------------------------------------

fn build_route_table() -> TrieNode<Vec<(AlpnMatcher, String)>> {
    let mut routes = TrieNode::root();
    routes.domain_insert(
        b"exact.example.com".to_vec(),
        vec![(AlpnMatcher::Any, "cluster-exact-any".to_owned())],
    );
    routes.domain_insert(
        b"*.wild.example.com".to_vec(),
        vec![(AlpnMatcher::Any, "cluster-wild".to_owned())],
    );
    routes.domain_insert(
        b"prefs.example.com".to_vec(),
        vec![
            (one_of(&[b"http/1.1"]), "cluster-http11".to_owned()),
            (one_of(&[b"h2"]), "cluster-h2".to_owned()),
        ],
    );
    routes.domain_insert(
        b"mixed.example.com".to_vec(),
        vec![
            (one_of(&[b"h2"]), "cluster-h2mixed".to_owned()),
            (AlpnMatcher::Any, "cluster-default-mixed".to_owned()),
        ],
    );
    routes.domain_insert(
        b"alpn.example.com".to_vec(),
        vec![(one_of(&[b"h2"]), "cluster-h2-only".to_owned())],
    );
    routes.domain_insert(
        b"split.example.com".to_vec(),
        vec![(AlpnMatcher::Any, "cluster-split".to_owned())],
    );
    routes
}

// --------------------------------------------------------------------------
// Scenario grammar: a wire, a `PrereadConfig`'s worth of parameters, a
// fragmentation schedule, and (usually) an expected terminal `Output`.
// --------------------------------------------------------------------------

/// How a scenario's wire is handed to `SniPrereadCore::handle_input`.
enum Delivery {
    /// The whole wire in one `Input::Bytes` call.
    OneShot,
    /// One byte at a time -- the heaviest fragmentation schedule.
    OneByteDrip,
    /// `n` random increasing split points, ending at the full length.
    RandomSplits(usize),
}

/// What to feed if the wire itself never completes a decision.
#[derive(Clone, Copy)]
enum FinalizeKind {
    Timeout,
    FrontClosed,
}

struct Scenario {
    wire: Vec<u8>,
    inbound_proxy: bool,
    max_bytes: usize,
    timeout: Duration,
    accept_wildcard: bool,
    delivery: Delivery,
    finalize_with: FinalizeKind,
    /// `Some(_)` for directed scenarios with a known-correct terminal;
    /// `None` for the chaos generator (and anything buggify has mutated),
    /// where only "no panic + invariants hold" is asserted.
    expected: Option<Output>,
    /// Tags a wire that spans more than one TLS record on the wire.
    is_multi_record: bool,
    /// Tags the `gen_complete_over_cap_routes` class: a COMPLETE hello with
    /// trailing bytes that push the window past `max_bytes`, which must
    /// still route -- see [`CoverageTally::complete_over_cap`].
    is_complete_over_cap: bool,
    tag: &'static str,
}

impl Scenario {
    fn new(wire: Vec<u8>, expected: Option<Output>, tag: &'static str) -> Self {
        Scenario {
            wire,
            inbound_proxy: false,
            max_bytes: 16 * 1024,
            timeout: Duration::from_secs(3),
            accept_wildcard: true,
            delivery: Delivery::OneShot,
            finalize_with: FinalizeKind::Timeout,
            expected,
            is_multi_record: false,
            is_complete_over_cap: false,
            tag,
        }
    }
}

/// Randomize the delivery granularity for a wire of the given length. Most
/// connections go one-shot or a few random splits; a fraction go full
/// one-byte drip (bounded to smallish wires so the sweep stays moderate).
fn pick_delivery(ctx: &SimContext, wire_len: usize) -> Delivery {
    if wire_len <= 1 {
        return Delivery::OneShot;
    }
    match ctx.random().random_range(0..100u32) {
        0..55 => Delivery::OneShot,
        55..80 => Delivery::RandomSplits(ctx.random().random_range(2..6usize)),
        _ => {
            if wire_len <= 400 {
                Delivery::OneByteDrip
            } else {
                Delivery::RandomSplits(4)
            }
        }
    }
}

// --------------------------------------------------------------------------
// Directed generators -- one per required coverage class, plus a chaos one.
// Each guarantees reachability of its class BY CONSTRUCTION; `generate_scenario`
// mixes them with weights so the sweep hits every class many times over.
// --------------------------------------------------------------------------

fn gen_accept_exact_any(ctx: &SimContext) -> Scenario {
    let alpn_choice: Vec<&[u8]> = match ctx.random().random_range(0..3u8) {
        0 => vec![],
        1 => vec![b"h2"],
        _ => vec![b"foo", b"h2"],
    };
    let wire = if alpn_choice.is_empty() {
        hello_no_alpn("exact.example.com")
    } else {
        hello("exact.example.com", &alpn_choice)
    };
    let expected = routed(
        "cluster-exact-any",
        "exact.example.com",
        &alpn_choice,
        0,
        None,
    );
    let mut s = Scenario::new(wire, Some(expected), "accept_exact_any");
    s.delivery = pick_delivery(ctx, s.wire.len());
    s
}

fn gen_accept_wildcard(ctx: &SimContext) -> Scenario {
    let n: u32 = ctx.random().random_range(0..1000u32);
    let sni = format!("host{n}.wild.example.com");
    let with_alpn = ctx.random().random_bool(0.5);
    let wire = if with_alpn {
        hello(&sni, &[b"h2"])
    } else {
        hello_no_alpn(&sni)
    };
    let expected = routed(
        "cluster-wild",
        &sni,
        if with_alpn { &[b"h2"] } else { &[] },
        0,
        None,
    );
    let mut s = Scenario::new(wire, Some(expected), "accept_wildcard");
    s.delivery = pick_delivery(ctx, s.wire.len());
    s
}

fn gen_accept_alpn_first_pref(ctx: &SimContext) -> Scenario {
    let h2_first = ctx.random().random_bool(0.5);
    let (offer, expected_cluster): (&[&[u8]], &str) = if h2_first {
        (&[b"h2", b"http/1.1"], "cluster-h2")
    } else {
        (&[b"http/1.1", b"h2"], "cluster-http11")
    };
    let wire = hello("prefs.example.com", offer);
    let expected = routed(expected_cluster, "prefs.example.com", offer, 0, None);
    let mut s = Scenario::new(wire, Some(expected), "accept_alpn_first_pref");
    s.delivery = pick_delivery(ctx, s.wire.len());
    s
}

fn gen_accept_alpn_catch_all(ctx: &SimContext) -> Scenario {
    let wire = hello("mixed.example.com", &[b"spdy/1"]);
    let expected = routed(
        "cluster-default-mixed",
        "mixed.example.com",
        &[b"spdy/1"],
        0,
        None,
    );
    let mut s = Scenario::new(wire, Some(expected), "accept_alpn_catch_all");
    s.delivery = pick_delivery(ctx, s.wire.len());
    s
}

fn gen_accept_no_alpn_catch_all(ctx: &SimContext) -> Scenario {
    let wire = hello_no_alpn("mixed.example.com");
    let expected = routed("cluster-default-mixed", "mixed.example.com", &[], 0, None);
    let mut s = Scenario::new(wire, Some(expected), "accept_no_alpn_catch_all");
    s.delivery = pick_delivery(ctx, s.wire.len());
    s
}

fn gen_accept_mixed_case(ctx: &SimContext) -> Scenario {
    const VARIANTS: [&str; 3] = [
        "EXACT.example.com",
        "Exact.Example.COM",
        "eXaCt.eXaMpLe.CoM",
    ];
    let raw = VARIANTS[ctx.random().random_range(0..VARIANTS.len())];
    let wire = hello_no_alpn(raw);
    let expected = routed("cluster-exact-any", "exact.example.com", &[], 0, None);
    let mut s = Scenario::new(wire, Some(expected), "accept_mixed_case_sni");
    s.delivery = pick_delivery(ctx, s.wire.len());
    s
}

fn gen_accept_trailing_dot(ctx: &SimContext) -> Scenario {
    let wire = hello_no_alpn("exact.example.com.");
    let expected = routed("cluster-exact-any", "exact.example.com", &[], 0, None);
    let mut s = Scenario::new(wire, Some(expected), "accept_trailing_dot_sni");
    s.delivery = pick_delivery(ctx, s.wire.len());
    s
}

fn gen_accept_with_grease(ctx: &SimContext) -> Scenario {
    let wire = build_client_hello_wire(&[
        encode_grease_extension(ctx),
        encode_sni_extension("exact.example.com"),
        encode_alpn_extension(&[b"h2"]),
        encode_grease_extension(ctx),
    ]);
    let expected = routed("cluster-exact-any", "exact.example.com", &[b"h2"], 0, None);
    let mut s = Scenario::new(wire, Some(expected), "accept_with_grease");
    s.delivery = pick_delivery(ctx, s.wire.len());
    s
}

fn gen_accept_multi_record(ctx: &SimContext) -> Scenario {
    let wire_full = hello("split.example.com", &[b"h2"]);
    let chunks: usize = ctx.random().random_range(2..7usize);
    let split = split_into_records(&wire_full, chunks);
    let expected = routed("cluster-split", "split.example.com", &[b"h2"], 0, None);
    let mut s = Scenario::new(split, Some(expected), "accept_multi_record");
    s.is_multi_record = true;
    s.delivery = pick_delivery(ctx, s.wire.len());
    s
}

/// Forces one-byte-drip delivery of an otherwise-ordinary accepted hello --
/// a deterministic, RNG-independent guarantee that `fragmented_delivery`
/// coverage is non-zero, on top of whatever `pick_delivery` organically
/// selects elsewhere.
fn gen_accept_one_byte_drip(_ctx: &SimContext) -> Scenario {
    let wire = hello_no_alpn("exact.example.com");
    let expected = routed("cluster-exact-any", "exact.example.com", &[], 0, None);
    let mut s = Scenario::new(wire, Some(expected), "accept_one_byte_drip");
    s.delivery = Delivery::OneByteDrip;
    s
}

fn gen_sni_unmatched(ctx: &SimContext) -> Scenario {
    let n: u32 = ctx.random().random_range(0..1_000_000u32);
    let sni = format!("unknown-{n}.example.net");
    let wire = hello_no_alpn(&sni);
    let mut s = Scenario::new(
        wire,
        Some(Output::Reject(RejectReason::SniUnmatched)),
        "sni_unmatched",
    );
    s.delivery = pick_delivery(ctx, s.wire.len());
    s
}

fn gen_alpn_unmatched(ctx: &SimContext) -> Scenario {
    const OFFERS: [&[&[u8]]; 3] = [&[b"http/1.1"], &[b"spdy/1", b"ftp"], &[b"http/1.1", b"foo"]];
    let offer = OFFERS[ctx.random().random_range(0..OFFERS.len())];
    let wire = hello("alpn.example.com", offer);
    let mut s = Scenario::new(
        wire,
        Some(Output::Reject(RejectReason::AlpnUnmatched)),
        "alpn_unmatched",
    );
    s.delivery = pick_delivery(ctx, s.wire.len());
    s
}

fn gen_no_sni(ctx: &SimContext) -> Scenario {
    let wire = build_client_hello_wire(&[]);
    let mut s = Scenario::new(wire, Some(Output::Reject(RejectReason::NoSni)), "no_sni");
    s.delivery = pick_delivery(ctx, s.wire.len());
    s
}

fn gen_ech_outer_absent(ctx: &SimContext) -> Scenario {
    let payload_len: usize = ctx.random().random_range(1..8usize);
    let payload = vec![0u8; payload_len];
    let wire = build_client_hello_wire(&[encode_extension(EXT_ECH, &payload)]);
    let mut s = Scenario::new(
        wire,
        Some(Output::Reject(RejectReason::EchOuterAbsent)),
        "ech_outer_absent",
    );
    s.delivery = pick_delivery(ctx, s.wire.len());
    s
}

fn gen_not_tls(ctx: &SimContext) -> Scenario {
    const NON_HANDSHAKE_TYPES: [u8; 4] = [20, 21, 23, 24];
    let content_type = NON_HANDSHAKE_TYPES[ctx.random().random_range(0..NON_HANDSHAKE_TYPES.len())];
    let wire = wrap_record(content_type, &[]);
    let mut s = Scenario::new(wire, Some(Output::Reject(RejectReason::NotTls)), "not_tls");
    s.delivery = pick_delivery(ctx, s.wire.len());
    s
}

fn gen_malformed_record_oversized(ctx: &SimContext) -> Scenario {
    let extra: u16 = ctx.random().random_range(0..100u16);
    let len = MAX_TLS_RECORD_LEN as u16 + 1 + extra;
    let mut wire = vec![TLS_CONTENT_TYPE_HANDSHAKE, 0x03, 0x03];
    wire.extend_from_slice(&len.to_be_bytes());
    let mut s = Scenario::new(
        wire,
        Some(Output::Reject(RejectReason::MalformedRecord)),
        "malformed_record_oversized",
    );
    s.delivery = pick_delivery(ctx, s.wire.len());
    s
}

/// Corrupts the SECOND record's `ContentType` mid multi-record reassembly --
/// the "a later record breaks an in-progress ClientHello" MalformedRecord
/// path, distinct from the oversized-length-on-the-first-record path above.
fn gen_malformed_record_mid_reassembly(_ctx: &SimContext) -> Scenario {
    let wire = hello_no_alpn("exact.example.com");
    let split = split_into_records(&wire, 3);
    let first_len = u16::from_be_bytes([split[3], split[4]]) as usize;
    let second_record_offset = 5 + first_len;
    let mut corrupted = split;
    corrupted[second_record_offset] = 23; // application_data, breaking reassembly
    let mut s = Scenario::new(
        corrupted,
        Some(Output::Reject(RejectReason::MalformedRecord)),
        "malformed_record_mid_reassembly",
    );
    s.is_multi_record = true;
    // Delivered one-shot: the corruption lives at a specific byte offset
    // computed above, and a one-shot feed keeps the trigger unambiguous.
    s.delivery = Delivery::OneShot;
    s
}

fn gen_malformed_handshake(ctx: &SimContext) -> Scenario {
    let hs = wrap_handshake_with_type(2, &[]); // ServerHello, not ClientHello
    let wire = wrap_record(TLS_CONTENT_TYPE_HANDSHAKE, &hs);
    let mut s = Scenario::new(
        wire,
        Some(Output::Reject(RejectReason::MalformedHandshake)),
        "malformed_handshake_wrong_type",
    );
    s.delivery = pick_delivery(ctx, s.wire.len());
    s
}

/// The genuine `TooLarge` case: a strict, INCOMPLETE prefix of a
/// well-formed ClientHello (any cut in `[1, full.len())` is guaranteed
/// `NeedMore` on its own -- see `truncated_incomplete_wire`'s doc for why a
/// truncation of a well-formed wire can never accidentally parse as
/// complete-at-a-smaller-size), capped at or below that prefix's own
/// length. The accumulated window therefore reaches `max_bytes` while the
/// parser still says `NeedMore`, which is the ONLY condition under which
/// the core rejects `TooLarge` (see `need_more_or_too_large` in
/// `tcp_preread/mod.rs`) -- a COMPLETE hello routes regardless of trailing
/// bytes past the cap, which is `gen_complete_over_cap_routes`'s class, not
/// this one.
fn gen_too_large(ctx: &SimContext) -> Scenario {
    let full = hello_no_alpn("exact.example.com");
    let cut = ctx.random().random_range(1..full.len());
    let wire = full[..cut].to_vec();
    let max_bytes: usize = ctx.random().random_range(1..wire.len() + 1);
    let mut s = Scenario::new(
        wire,
        Some(Output::Reject(RejectReason::TooLarge)),
        "too_large",
    );
    s.max_bytes = max_bytes;
    s.delivery = pick_delivery(ctx, s.wire.len());
    s
}

/// Regression coverage for the `on_bytes` reorder (Defect B fix): a
/// COMPLETE ClientHello followed by trailing bytes (e.g. the rest of the
/// handshake / early data) that push the accumulated window past
/// `max_bytes` must still ROUTE -- the cap only ever gates a would-be
/// `NeedMore` path, never a completed parse. `max_bytes` is pinned strictly
/// between the hello's own length and the full (hello + trailing) length,
/// mirroring the core-level `complete_hello_with_trailing_bytes_over_cap_routes`
/// unit test in `tcp_preread/mod.rs`. Without this directed class, a
/// regression that re-introduced checking the cap before completeness
/// would hide inside ordinary `accepted` coverage.
fn gen_complete_over_cap_routes(ctx: &SimContext) -> Scenario {
    let hello_wire = hello_no_alpn("exact.example.com");
    let trailing_len = ctx.random().random_range(1..64usize);
    let mut wire = hello_wire.clone();
    wire.extend((0..trailing_len).map(|_| ctx.random().random::<u8>()));
    let expected = routed("cluster-exact-any", "exact.example.com", &[], 0, None);
    let mut s = Scenario::new(wire, Some(expected), "complete_over_cap_routes");
    // Strictly between the hello's own length (inclusive) and the total
    // wire length (exclusive), so the window is never treated as
    // incomplete-at-cap yet the total wire still exceeds the cap.
    s.max_bytes = ctx
        .random()
        .random_range(hello_wire.len()..hello_wire.len() + trailing_len);
    s.is_complete_over_cap = true;
    s.delivery = pick_delivery(ctx, s.wire.len());
    s
}

fn gen_proxy_header_invalid(_ctx: &SimContext) -> Scenario {
    // 16 garbage bytes: long enough not to be `Incomplete` against the
    // 12-byte PROXY-v2 signature, but not a valid header (mirrors the
    // core's own `proxy_header_invalid_is_reachable` unit test).
    let wire = vec![0xAAu8; 16];
    let mut s = Scenario::new(
        wire,
        Some(Output::Reject(RejectReason::ProxyHeaderInvalid)),
        "proxy_header_invalid",
    );
    s.inbound_proxy = true;
    s.delivery = Delivery::OneShot;
    s
}

fn gen_proxy_prefixed_valid(ctx: &SimContext) -> Scenario {
    let src = SocketAddr::new(
        IpAddr::V4(Ipv4Addr::new(
            203,
            0,
            113,
            ctx.random().random_range(1..255u8),
        )),
        ctx.random().random_range(1024..65535u16),
    );
    let dst = SocketAddr::new(IpAddr::V4(Ipv4Addr::new(198, 51, 100, 9)), 443);
    let header = HeaderV2::new(Command::Proxy, src, dst).into_bytes();
    let hello_wire = hello_no_alpn("exact.example.com");
    let mut wire = header.clone();
    wire.extend_from_slice(&hello_wire);
    let expected = routed(
        "cluster-exact-any",
        "exact.example.com",
        &[],
        header.len(),
        Some(src),
    );
    let mut s = Scenario::new(wire, Some(expected), "proxy_prefixed_valid");
    s.inbound_proxy = true;
    s.delivery = pick_delivery(ctx, s.wire.len());
    s
}

/// Builds a strict, random-length prefix of an otherwise-valid ClientHello --
/// guaranteed to `NeedMore` on its own for any cut in `[0, len)` (see the
/// module doc for why a plain truncation of a well-formed wire can never
/// prematurely reject).
fn truncated_incomplete_wire(ctx: &SimContext, force_empty: bool) -> Vec<u8> {
    let full = hello("exact.example.com", &[b"h2"]);
    let cut = if force_empty {
        0
    } else {
        ctx.random().random_range(0..full.len())
    };
    full[..cut].to_vec()
}

fn gen_fragmented_timeout(ctx: &SimContext) -> Scenario {
    let wire = truncated_incomplete_wire(ctx, false);
    let mut s = Scenario::new(
        wire,
        Some(Output::Reject(RejectReason::Fragmented)),
        "fragmented_timeout",
    );
    s.delivery = Delivery::OneShot;
    s.finalize_with = FinalizeKind::Timeout;
    s
}

fn gen_front_closed_now(ctx: &SimContext) -> Scenario {
    let force_empty = ctx.random().random_bool(0.3);
    let wire = truncated_incomplete_wire(ctx, force_empty);
    let mut s = Scenario::new(
        wire,
        Some(Output::Reject(RejectReason::FrontClosed)),
        "front_closed_now",
    );
    s.delivery = Delivery::OneShot;
    s.finalize_with = FinalizeKind::FrontClosed;
    s
}

/// Unconstrained chaos: fully random bytes of random length, occasionally
/// seeded with a real hello prefix so the mutation sometimes still touches
/// deep parser state instead of always dying at the record header. No
/// expected outcome -- only "no panic + invariants hold" is asserted.
fn gen_random_mutation_chaos(ctx: &SimContext) -> Scenario {
    let len: usize = ctx.random().random_range(0..300usize);
    let mut wire = vec![0u8; len];
    for b in wire.iter_mut() {
        *b = ctx.random().random();
    }
    if !wire.is_empty() && ctx.random().random_bool(0.3) {
        let seed_hello = hello("chaos.example.com", &[b"h2"]);
        let n = seed_hello.len().min(wire.len());
        wire[..n].copy_from_slice(&seed_hello[..n]);
    }
    let mut s = Scenario::new(wire, None, "random_mutation_chaos");
    s.delivery = pick_delivery(ctx, s.wire.len());
    s
}

/// Draw a weighted-random scenario (weights sum to 134). Every required
/// coverage class has a dedicated, non-zero-weight generator so reachability
/// never depends on pure luck; `RandomMutationChaos` soaks up the remainder.
fn generate_scenario(ctx: &SimContext) -> Scenario {
    match ctx.random().random_range(0..134u32) {
        0..10 => gen_accept_exact_any(ctx),
        10..18 => gen_accept_wildcard(ctx),
        18..26 => gen_accept_alpn_first_pref(ctx),
        26..32 => gen_accept_alpn_catch_all(ctx),
        32..37 => gen_accept_no_alpn_catch_all(ctx),
        37..42 => gen_accept_mixed_case(ctx),
        42..47 => gen_accept_trailing_dot(ctx),
        47..51 => gen_accept_with_grease(ctx),
        51..57 => gen_accept_multi_record(ctx),
        57..63 => gen_accept_one_byte_drip(ctx),
        63..69 => gen_sni_unmatched(ctx),
        69..75 => gen_alpn_unmatched(ctx),
        75..80 => gen_no_sni(ctx),
        80..85 => gen_ech_outer_absent(ctx),
        85..90 => gen_not_tls(ctx),
        90..93 => gen_malformed_record_oversized(ctx),
        93..96 => gen_malformed_record_mid_reassembly(ctx),
        96..100 => gen_malformed_handshake(ctx),
        100..104 => gen_too_large(ctx),
        104..108 => gen_complete_over_cap_routes(ctx),
        108..111 => gen_proxy_header_invalid(ctx),
        111..115 => gen_proxy_prefixed_valid(ctx),
        115..120 => gen_fragmented_timeout(ctx),
        120..124 => gen_front_closed_now(ctx),
        _ => gen_random_mutation_chaos(ctx),
    }
}

// --------------------------------------------------------------------------
// Delivery execution + per-connection invariant checks.
// --------------------------------------------------------------------------

fn fnv1a(bytes: &[u8]) -> u64 {
    let mut hash: u64 = 0xcbf29ce484222325;
    for &b in bytes {
        hash ^= b as u64;
        hash = hash.wrapping_mul(0x100000001b3);
    }
    hash
}

/// The increasing prefix lengths a `Delivery` schedule feeds, always ending
/// at the full wire length -- matching `Input::Bytes`'s "always the FULL
/// accumulated window" contract (never a delta).
fn delivery_prefixes(ctx: &SimContext, len: usize, delivery: &Delivery) -> Vec<usize> {
    if len == 0 {
        return vec![0];
    }
    match delivery {
        Delivery::OneShot => vec![len],
        Delivery::OneByteDrip => (1..=len).collect(),
        Delivery::RandomSplits(n) => {
            let n = (*n).clamp(1, len);
            let mut points: Vec<usize> = (0..n.saturating_sub(1))
                .map(|_| ctx.random().random_range(1..len))
                .collect();
            points.push(len);
            points.sort_unstable();
            points.dedup();
            points
        }
    }
}

/// Feed a wire to a fresh core following the given (increasing, ending at
/// `wire.len()`) prefix schedule, stopping as soon as a terminal `Output` is
/// reached. Returns the last output seen (terminal, or `NeedMore` if the
/// whole schedule was exhausted without deciding) and whether more than one
/// `Bytes` call was needed. Takes an already-computed `prefixes` slice
/// (rather than a `Delivery` + RNG) so a caller can replay the EXACT same
/// schedule against a second core -- see `run_connection`'s determinism
/// cross-check for why that matters.
fn feed_with_delivery(
    ctx: &SimContext,
    base: Instant,
    core: &mut SniPrereadCore,
    cfg: &PrereadConfig<'_>,
    wire: &[u8],
    prefixes: &[usize],
) -> (Output, bool) {
    let mut saw_needmore = false;
    let mut prev_deadline: Option<Instant> = None;
    let mut last = Output::NeedMore { deadline: base };

    for &p in prefixes {
        let window = &wire[..p];
        let before = fnv1a(window);
        let now = base + ctx.time().now();
        let out = core.handle_input(cfg, Input::Bytes { buf: window, now });
        let after = fnv1a(window);
        assert_eq!(
            before,
            after,
            "seed={:#x}: byte window mutated by handle_input ({} bytes fed)",
            current_sim_seed(),
            window.len(),
        );

        if let Output::NeedMore { deadline } = out {
            saw_needmore = true;
            if let Some(prev) = prev_deadline {
                assert_eq!(
                    deadline,
                    prev,
                    "seed={:#x}: preread deadline moved across NeedMore outputs",
                    current_sim_seed(),
                );
            }
            prev_deadline = Some(deadline);
            last = Output::NeedMore { deadline };
            continue;
        }
        last = out;
        break;
    }
    (last, saw_needmore)
}

/// Feed a genuinely-undecided connection its out-of-band terminal signal
/// (`Timeout` or `FrontClosed`) if -- and only if -- it is still `NeedMore`
/// after its delivery schedule ran out. A no-op (returns `terminal`
/// unchanged) for any connection that already decided on the bytes alone.
async fn finalize_if_needed(
    ctx: &SimContext,
    base: Instant,
    core: &mut SniPrereadCore,
    cfg: &PrereadConfig<'_>,
    terminal: Output,
    finalize_with: FinalizeKind,
    scenario_timeout: Duration,
) -> Output {
    if !matches!(terminal, Output::NeedMore { .. }) {
        return terminal;
    }
    match finalize_with {
        FinalizeKind::Timeout => {
            let _ = ctx
                .time()
                .sleep(scenario_timeout + Duration::from_millis(50))
                .await;
            let now = base + ctx.time().now();
            core.handle_input(cfg, Input::Timeout { now })
        }
        FinalizeKind::FrontClosed => core.handle_input(cfg, Input::FrontClosed),
    }
}

/// Drive one full connection: deliver the wire, finalize with the scenario's
/// out-of-band signal if it never decided on its own, check the directed
/// expectation (when present), then run the replay-identical and
/// replay-determinism cross-checks. Returns the connection's terminal output
/// and whether its delivery needed more than one `Bytes` call.
async fn run_connection(
    ctx: &SimContext,
    base: Instant,
    routes: &TrieNode<Vec<(AlpnMatcher, String)>>,
    scenario: &Scenario,
    conn_idx: usize,
) -> (Output, bool) {
    let seed = current_sim_seed();
    let cfg = PrereadConfig {
        routes,
        inbound_proxy: scenario.inbound_proxy,
        max_bytes: scenario.max_bytes,
        timeout: scenario.timeout,
        accept_wildcard: scenario.accept_wildcard,
    };

    // Computed ONCE so every core below (the primary run and the
    // determinism cross-check) sees the EXACT same sequence of buffer
    // views. Fragmentation granularity can legitimately change the
    // outcome -- e.g. a small under-`max_bytes` prefix can decide
    // something on its own content before the wire ever grows past the
    // cap, where a one-shot feed of the full (over-cap) wire would hit
    // `TooLarge` on its very first call -- so replaying a DIFFERENT
    // schedule (or a plain one-shot) against a second core is not a valid
    // determinism check; replaying the IDENTICAL schedule is.
    let prefixes = delivery_prefixes(ctx, scenario.wire.len(), &scenario.delivery);

    let mut core = SniPrereadCore::new();
    let (raw_terminal, saw_needmore) =
        feed_with_delivery(ctx, base, &mut core, &cfg, &scenario.wire, &prefixes);
    let terminal = finalize_if_needed(
        ctx,
        base,
        &mut core,
        &cfg,
        raw_terminal,
        scenario.finalize_with,
        scenario.timeout,
    )
    .await;

    debug_assert!(
        !matches!(terminal, Output::NeedMore { .. }),
        "seed={seed:#x} conn={conn_idx} scenario={}: connection must reach a terminal verdict",
        scenario.tag,
    );

    if let Some(expected) = &scenario.expected {
        assert_eq!(
            &terminal, expected,
            "seed={seed:#x} conn={conn_idx} scenario={}: expected {expected:?}, got {terminal:?}",
            scenario.tag,
        );
    }

    // Terminal-at-most-once + latched replay identical: a redundant call of
    // a randomly chosen Input kind must replay the SAME terminal output.
    let redundant_kind = ctx.random().random_range(0..3u8);
    let replay = match redundant_kind {
        0 => {
            let before = fnv1a(&scenario.wire);
            let now = base + ctx.time().now();
            let out = core.handle_input(
                &cfg,
                Input::Bytes {
                    buf: &scenario.wire,
                    now,
                },
            );
            let after = fnv1a(&scenario.wire);
            assert_eq!(
                before, after,
                "seed={seed:#x} conn={conn_idx}: byte window mutated on the replay call",
            );
            out
        }
        1 => {
            let now = base + ctx.time().now();
            core.handle_input(&cfg, Input::Timeout { now })
        }
        _ => core.handle_input(&cfg, Input::FrontClosed),
    };
    assert_eq!(
        replay, terminal,
        "seed={seed:#x} conn={conn_idx} scenario={}: decided latch did not replay identically \
         (redundant_kind={redundant_kind})",
        scenario.tag,
    );

    // Replay determinism: a FRESH core fed the EXACT same (bytes, schedule,
    // config) must reach the exact same terminal. This is a regression
    // guard against a hidden nondeterministic dependency (wall clock,
    // unseeded rand, hash-map iteration) leaking into the core -- it is
    // trivially true today by construction, same as `udp_simulation_is_deterministic`.
    let mut core2 = SniPrereadCore::new();
    let (raw_terminal2, _) =
        feed_with_delivery(ctx, base, &mut core2, &cfg, &scenario.wire, &prefixes);
    let terminal2 = finalize_if_needed(
        ctx,
        base,
        &mut core2,
        &cfg,
        raw_terminal2,
        scenario.finalize_with,
        scenario.timeout,
    )
    .await;
    assert_eq!(
        terminal2, terminal,
        "seed={seed:#x} conn={conn_idx} scenario={}: replay determinism violated -- identical \
         (bytes, delivery schedule, config) produced a different terminal on a fresh core",
        scenario.tag,
    );

    (terminal, saw_needmore)
}

// --------------------------------------------------------------------------
// Coverage tally: the hard per-class reachability gate.
// --------------------------------------------------------------------------

#[derive(Debug, Default, Clone, Copy)]
struct CoverageTally {
    accepted: u64,
    not_tls: u64,
    malformed_record: u64,
    malformed_handshake: u64,
    fragmented: u64,
    too_large: u64,
    no_sni: u64,
    ech_outer_absent: u64,
    sni_unmatched: u64,
    alpn_unmatched: u64,
    proxy_header_invalid: u64,
    front_closed: u64,
    fragmented_delivery: u64,
    multi_record: u64,
    /// Connections from [`gen_complete_over_cap_routes`]: a COMPLETE hello
    /// with trailing bytes past `max_bytes` that correctly routed anyway.
    complete_over_cap: u64,
}

impl CoverageTally {
    fn record_outcome(&mut self, out: &Output) {
        match out {
            Output::Routed { .. } => self.accepted += 1,
            Output::Reject(reason) => match reason {
                RejectReason::NotTls => self.not_tls += 1,
                RejectReason::MalformedRecord => self.malformed_record += 1,
                RejectReason::MalformedHandshake => self.malformed_handshake += 1,
                RejectReason::Fragmented => self.fragmented += 1,
                RejectReason::TooLarge => self.too_large += 1,
                RejectReason::NoSni => self.no_sni += 1,
                RejectReason::EchOuterAbsent => self.ech_outer_absent += 1,
                RejectReason::SniUnmatched => self.sni_unmatched += 1,
                RejectReason::AlpnUnmatched => self.alpn_unmatched += 1,
                RejectReason::ProxyHeaderInvalid => self.proxy_header_invalid += 1,
                RejectReason::FrontClosed => self.front_closed += 1,
            },
            Output::NeedMore { .. } => {
                debug_assert!(
                    false,
                    "record_outcome expects a terminal Output, got NeedMore"
                );
            }
        }
    }

    fn merge(&mut self, other: &CoverageTally) {
        self.accepted += other.accepted;
        self.not_tls += other.not_tls;
        self.malformed_record += other.malformed_record;
        self.malformed_handshake += other.malformed_handshake;
        self.fragmented += other.fragmented;
        self.too_large += other.too_large;
        self.no_sni += other.no_sni;
        self.ech_outer_absent += other.ech_outer_absent;
        self.sni_unmatched += other.sni_unmatched;
        self.alpn_unmatched += other.alpn_unmatched;
        self.proxy_header_invalid += other.proxy_header_invalid;
        self.front_closed += other.front_closed;
        self.fragmented_delivery += other.fragmented_delivery;
        self.multi_record += other.multi_record;
        self.complete_over_cap += other.complete_over_cap;
    }

    /// The hard gate: every class must have been exercised at least once
    /// across the whole sweep. A sweep that never reaches one of these is a
    /// coverage regression, not a pass.
    fn assert_full_coverage(&self) {
        let checks: [(&str, u64); 15] = [
            ("accepted", self.accepted),
            ("not_tls", self.not_tls),
            ("malformed_record", self.malformed_record),
            ("malformed_handshake", self.malformed_handshake),
            ("fragmented", self.fragmented),
            ("too_large", self.too_large),
            ("no_sni", self.no_sni),
            ("ech_outer_absent", self.ech_outer_absent),
            ("sni_unmatched", self.sni_unmatched),
            ("alpn_unmatched", self.alpn_unmatched),
            ("proxy_header_invalid", self.proxy_header_invalid),
            ("front_closed", self.front_closed),
            ("fragmented_delivery", self.fragmented_delivery),
            ("multi_record", self.multi_record),
            ("complete_over_cap", self.complete_over_cap),
        ];
        let zero: Vec<&str> = checks
            .iter()
            .filter(|(_, n)| *n == 0)
            .map(|(name, _)| *name)
            .collect();
        assert!(
            zero.is_empty(),
            "coverage gate failed -- zero occurrences of: {zero:?}\nfull tally: {self:?}",
        );
    }
}

/// Folds one connection's terminal `Output` into a running per-seed
/// fingerprint, for the determinism guard (two runs of the same seed must
/// produce the same fold).
fn fold_fingerprint(acc: u64, out: &Output) -> u64 {
    let tag: u64 = match out {
        Output::Routed {
            cluster,
            content_offset,
            proxy_source,
            sni,
            alpn,
        } => {
            let mut h = fnv1a(cluster.as_bytes()) ^ fnv1a(sni.as_bytes());
            h ^= *content_offset as u64;
            if let Some(addr) = proxy_source {
                h ^= fnv1a(addr.to_string().as_bytes());
            }
            for p in alpn {
                h ^= fnv1a(p);
            }
            h ^ 0xA5
        }
        Output::Reject(reason) => (*reason as u64).wrapping_add(1) ^ 0x5A,
        Output::NeedMore { .. } => {
            debug_assert!(
                false,
                "fold_fingerprint expects a terminal Output, got NeedMore"
            );
            0
        }
    };
    acc.rotate_left(7) ^ tag
}

// --------------------------------------------------------------------------
// The workload: one moonpool iteration == one deterministic seed run over
// `connections` independent per-connection scenarios.
// --------------------------------------------------------------------------

type CoverageSink = Arc<Mutex<CoverageTally>>;
/// One aggregate per-seed fingerprint pushed here, for the determinism guard.
type FingerprintSink = Arc<Mutex<Vec<u64>>>;

struct TcpPrereadSimWorkload {
    connections: usize,
    verbose: bool,
    coverage: Option<CoverageSink>,
    fingerprint_sink: Option<FingerprintSink>,
}

#[async_trait]
impl Workload for TcpPrereadSimWorkload {
    fn name(&self) -> &'static str {
        "tcp_preread_simulation"
    }

    async fn run(&mut self, ctx: &SimContext) -> SimulationResult<()> {
        let seed = current_sim_seed();
        let base = Instant::now();
        let routes = build_route_table();

        let mut local = CoverageTally::default();
        let mut fp: u64 = 0xcbf29ce484222325;

        for i in 0..self.connections {
            let mut scenario = generate_scenario(ctx);
            // Buggify: low-probability extra adversarial mutation, using
            // moonpool's fault-injection primitive. The directed expectation
            // is cleared unconditionally -- a bit-flip can change which
            // (if any) terminal a wire reaches, but `run_connection`'s
            // route-determinism check is a RUNTIME fact (whether finalize
            // was actually needed), so it stays correct for a mutated wire
            // regardless of which scenario class produced it.
            if !scenario.wire.is_empty() && buggify_with_prob!(0.02) {
                let idx: usize = ctx.random().random_range(0..scenario.wire.len());
                scenario.wire[idx] ^= 0xFF;
                scenario.expected = None;
            }

            let (terminal, saw_needmore) = run_connection(ctx, base, &routes, &scenario, i).await;
            local.record_outcome(&terminal);
            if saw_needmore {
                local.fragmented_delivery += 1;
            }
            if scenario.is_multi_record {
                local.multi_record += 1;
            }
            if scenario.is_complete_over_cap {
                local.complete_over_cap += 1;
            }
            if self.verbose && (i + 1) % 25 == 0 {
                eprintln!(
                    "seed={seed:#x} conn={i} scenario={} terminal={terminal:?}",
                    scenario.tag,
                );
            }
            fp = fold_fingerprint(fp, &terminal);
        }

        if self.verbose {
            eprintln!(
                "seed={seed:#x} DONE connections={} tally={local:?}",
                self.connections,
            );
        }
        if let Some(sink) = &self.coverage {
            sink.lock().unwrap().merge(&local);
        }
        if let Some(sink) = &self.fingerprint_sink {
            sink.lock().unwrap().push(fp);
        }

        Ok(())
    }
}

// --------------------------------------------------------------------------
// Env-knob parsing (decimal or 0x-hex) + run-time budget.
// --------------------------------------------------------------------------

fn parse_u64(s: &str) -> Option<u64> {
    let t = s.trim();
    if let Some(hex) = t.strip_prefix("0x").or_else(|| t.strip_prefix("0X")) {
        u64::from_str_radix(hex, 16).ok()
    } else {
        t.parse::<u64>().ok()
    }
}

fn env_u64(key: &str) -> Option<u64> {
    std::env::var(key).ok().and_then(|s| parse_u64(&s))
}

fn env_usize(key: &str) -> Option<usize> {
    env_u64(key).map(|v| v as usize)
}

/// A connection's cost is dominated by its fragmentation schedule (up to
/// one `handle_input` call per byte for one-byte-drip), not a fixed
/// per-step cost, so the budget scales with the connection count generously.
fn run_budget(connections: usize) -> Duration {
    Duration::from_secs((connections as u64).saturating_mul(20).saturating_add(600))
}

/// Panic with the full per-iteration error detail when any run failed.
fn assert_no_failures(report: &SimulationReport) {
    if report.failed_runs != 0 {
        let errs: Vec<String> = report
            .individual_metrics
            .iter()
            .filter_map(|r| r.as_ref().err().map(|e| format!("{e:?}")))
            .collect();
        panic!(
            "failed_runs={} seeds_failing={:?}\nassertion_violations={:?}\ncoverage_violations={:?}\nerrors:\n{}",
            report.failed_runs,
            report.seeds_failing,
            report.assertion_violations,
            report.coverage_violations,
            errs.join("\n---\n"),
        );
    }
}

// --------------------------------------------------------------------------
// Tests.
// --------------------------------------------------------------------------

/// Deterministic seed sweep. On a hard invariant violation the panic carries
/// the failing seed + connection index; moonpool also lists the seed in
/// `report.seeds_failing`. The default (no env override) sweep additionally
/// enforces the hard per-class coverage gate.
#[test]
fn tcp_preread_simulation_seed_sweep() {
    let connections = env_usize("SOZU_TCP_PREREAD_SIM_STEPS").unwrap_or(48);

    // Single-seed replay mode: no coverage gate (one seed need not reach
    // every class -- it's a debugging tool, not the sweep).
    if let Some(seed) = env_u64("SOZU_TCP_PREREAD_SIM_SEED") {
        eprintln!(
            "== TCP preread sim single-seed replay: seed={seed:#x} connections={connections} =="
        );
        let report = SimulationBuilder::new()
            .workload(TcpPrereadSimWorkload {
                connections,
                verbose: true,
                coverage: None,
                fingerprint_sink: None,
            })
            .set_debug_seeds(vec![seed])
            // Without an explicit iteration count, `SimulationBuilder`
            // defaults to `UntilCoverageStable` (up to 1000 iterations) and
            // would keep drawing FRESH random seeds after this one -- fixing
            // it to 1 is what actually makes this "replay that ONE seed",
            // matching the documented `SOZU_TCP_PREREAD_SIM_SEED` contract.
            .set_iterations(1)
            .run_time_budget(run_budget(connections))
            .run();
        assert_no_failures(&report);
        return;
    }

    let seeds = env_usize("SOZU_TCP_PREREAD_SIM_SEEDS").unwrap_or(256);
    let coverage: CoverageSink = Arc::new(Mutex::new(CoverageTally::default()));
    let report = SimulationBuilder::new()
        .workload(TcpPrereadSimWorkload {
            connections,
            verbose: false,
            coverage: Some(coverage.clone()),
            fingerprint_sink: None,
        })
        .set_iterations(seeds)
        .run_time_budget(run_budget(connections))
        .run();
    assert_no_failures(&report);

    let tally = *coverage.lock().unwrap();
    eprintln!(
        "tcp_preread_simulation_seed_sweep: seeds={seeds} connections_per_seed={connections} \
         tally={tally:?}"
    );
    tally.assert_full_coverage();
}

/// Fast smoke test pinning one representative seed.
#[test]
fn tcp_preread_simulation_replays_known_seed() {
    let connections = 64;
    let report = SimulationBuilder::new()
        .workload(TcpPrereadSimWorkload {
            connections,
            verbose: false,
            coverage: None,
            fingerprint_sink: None,
        })
        .set_debug_seeds(vec![0xFEED_ACE5])
        .set_iterations(1)
        .run_time_budget(run_budget(connections))
        .run();
    assert_no_failures(&report);
}

/// Determinism guard: the same seed yields an identical observable trace
/// (the folded per-connection terminal-output fingerprint). If this
/// diverges, the harness or the core gained a hidden nondeterministic
/// dependency (wall clock, unseeded rand, hash-map iteration leaking into
/// outputs).
#[test]
fn tcp_preread_simulation_is_deterministic() {
    fn fingerprint(seed: u64) -> u64 {
        let connections = 32;
        let sink: FingerprintSink = Arc::new(Mutex::new(Vec::new()));
        let report = SimulationBuilder::new()
            .workload(TcpPrereadSimWorkload {
                connections,
                verbose: false,
                coverage: None,
                fingerprint_sink: Some(sink.clone()),
            })
            .set_debug_seeds(vec![seed])
            .set_iterations(1)
            .run_time_budget(run_budget(connections))
            .run();
        assert_no_failures(&report);
        let v = sink.lock().unwrap();
        *v.first().expect("workload recorded a fingerprint")
    }

    let a = fingerprint(0x00AB_CDEF);
    let b = fingerprint(0x00AB_CDEF);
    assert_eq!(
        a, b,
        "same seed must yield an identical trace (determinism)"
    );
}

#[test]
fn env_parse_accepts_hex_and_decimal() {
    assert_eq!(parse_u64("42"), Some(42));
    assert_eq!(parse_u64(" 256 "), Some(256));
    assert_eq!(parse_u64("0xdeadbeef"), Some(0xdead_beef));
    assert_eq!(parse_u64("0XFF"), Some(0xFF));
    assert_eq!(parse_u64("notanumber"), None);
}
