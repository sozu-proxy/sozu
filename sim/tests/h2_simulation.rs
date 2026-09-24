// Gated on `--cfg tokio_unstable`: moonpool-sim seeds tokio's runtime RNG via the
// unstable `RngSeed` API. Without the flag this whole test crate compiles to an
// empty (0-test) binary and pulls no moonpool/tokio deps (see Cargo.toml), so a
// plain `cargo test --workspace` stays free of tokio_unstable. Run the real sweep
// with `RUSTFLAGS="--cfg tokio_unstable" cargo test -p sozu-sim --test h2_simulation`.
#![cfg(tokio_unstable)]
//! Deterministic simulation of the sans-io H2 core
//! (`sozu_lib::protocol::mux::ConnectionH2`) driven by the [moonpool-sim]
//! engine, on the pattern [`udp_simulation.rs`] and [`tcp_preread_sim.rs`]
//! established (`doc/testing.md`, "Recipe: adding a simulator over another
//! sans-io core"). This is step C4 of the `poll_timeout` series
//! ([#1359](https://github.com/sozu-proxy/sozu/issues/1359)).
//!
//! # How the core is driven, and why through a socket
//!
//! The byte-in / byte-out split is half-landed: [`ConnectionH2`] already has
//! `poll_read_target` / `handle_read` and the `h2_transmit::gather` /
//! `h2_transmit::confirm` write pair, but they are `pub(super)` and
//! `ConnectionH2` is still generic over `Front: SocketHandler` with a `socket`
//! field. So this harness takes the option this issue's Q10 discussion called
//! **(a)**: it supplies its OWN in-memory [`SocketHandler`] ([`SimSocket`]) and
//! drives the public `ConnectionH2::readable` / `ConnectionH2::writable` entry
//! points. Nothing here reaches into the core — every assertion is on bytes the
//! core emitted, on `Connection::poll_timeout`, or on `ConnectionH2::stream_count`.
//!
//! When the remaining `Front` touch points go and the read/write pair becomes
//! public, this harness **simplifies rather than breaks**: [`SimSocket`] and the
//! `mio` dev-dependency it exists to satisfy both disappear, the scenario grammar
//! and every assertion below stay exactly as they are, and only [`H2Harness::pump`]
//! is rewritten. That is the same "swap the driver, keep the scenarios" promise
//! `doc/testing.md` makes for each sub-machine extraction.
//!
//! `lib/` stays async-free — this crate is the only async home for the
//! simulation (moonpool/tokio are dev-dependencies, gated on `tokio_unstable`).
//!
//! # The determinism premise, measured
//!
//! A simulator over a core that still reads the host clock is *vacuously*
//! deterministic — trap #1 of the four [`metrics_lease_sim.rs`] names. The
//! premise here is that the H2 core takes **no** clock sample on the paths this
//! harness drives, and it is a measured fact rather than an assumption: against
//! `f0a45568`, the only `Instant::now()` in `h2.rs` outside a `#[cfg(test)]`
//! module is the one in `ConnectionH2::new`, documented there as "the one clock
//! sample this module takes outside `Mux`'s sampling points". Every other
//! time-based decision reads `ConnectionH2::now`, mirrored from
//! [`Context::now`] at each public entry point — which this harness sets from
//! moonpool's virtual clock before every call. The same holds for
//! `h2_drain.rs`, `h2_stream_table.rs` and `h2_flood_detector.rs`: their
//! `Instant::now()` hits are all inside their own `mod tests`.
//!
//! The one construction-time sample is neutralised by pinning it: the harness
//! captures `Connection::poll_timeout()` immediately after construction and
//! reports every later deadline as a delta from it, so the absolute base never
//! enters the trace.
//!
//! # What this simulator establishes that a unit test cannot
//!
//! Three named properties, each with its own `#[test]` on top of the sweep:
//!
//! 1. **Determinism** ([`h2_simulation_is_deterministic`]) — one seed replayed
//!    twice yields a byte-identical trace, and distinct seeds explore distinct
//!    interleavings. The trace is the *observable* history: frames out, stream
//!    count, published deadline, `MuxResult`. Nothing wall-clock, nothing
//!    address-shaped, nothing `Ulid`-shaped enters it.
//!
//! 2. **The wheel's earliness window is not an expiry**
//!    ([`h2_stream_idle_deadline_survives_the_wheel_earliness_window`]) —
//!    `crate::timer`'s `duration_to_tick` rounds a delay to the NEAREST tick, so
//!    an entry armed for `D` is delivered from `D - tick/2` onwards and the
//!    total earliness is `(delay_ms + tick_ms/2) mod tick_ms`, i.e. `[0, 99]` ms
//!    at the default 100 ms tick (`timer.rs::duration_to_tick`'s own
//!    documentation, pinned by `timer.rs`'s
//!    `test_maximum_earliness_is_a_full_half_tick`). This harness does not
//!    re-test the wheel — it takes that class as ground truth, GENERATES an
//!    earliness from it, and asserts the core-side consequence: driven at every
//!    instant in `[D - 99ms, D - 1ms]` the core emits no RST_STREAM and does not
//!    move its published deadline, and at `D` it emits exactly one. That is the
//!    claim C1's re-validation exists to protect, expressed on observable bytes
//!    instead of on `Mux::sync_timeout`'s internals.
//!
//! 3. **Multi-stream bounds and credit attribution under adversarial
//!    scheduling** ([`h2_concurrent_stream_ceiling_holds_under_adversarial_interleaving`],
//!    [`h2_inbound_credit_is_attributed_to_the_stream_that_spent_it`]) — the
//!    concurrent-stream ceiling, and the per-stream inbound flow-control credit
//!    the core returns. Both are checked against an oracle built from the
//!    harness's own CONSTRUCTION INPUTS ([`H2ConnectionConfig`]) and its own
//!    count of what it put on the wire, never from anything the core reported.
//!    Streams open, reset and recycle in a seed-chosen order while frame
//!    boundaries straddle `socket_read` calls and writes complete partially, so
//!    a DATA frame's 9-octet header routinely arrives across two reads — the
//!    interleaving space in which a mis-attributed credit would hide, and the
//!    one an end-to-end test reaches only by luck.
//!
//!    What is deliberately NOT asserted here is that the core refuses DATA past
//!    the connection-level receive window it advertised. It does not
//!    (sozu-proxy/sozu#1488): `h2_flow_control::H2FlowControl::window` is the
//!    SEND-side window ("peer-granted credit for OUR sends"), and the receive
//!    side is only `H2FlowControl::account_received_bytes`, a counter that
//!    decides when to hand credit back. No receive window is tracked and no
//!    `FLOW_CONTROL_ERROR` is raised, so a peer can push past what Sōzu
//!    advertised; back-pressure comes from the buffer pool instead. Writing an
//!    assertion loose enough to pass would certify that, so this file states the
//!    gap instead of encoding it.
//!
//! # Why the clock is the harness's and not moonpool's
//!
//! The other three simulators advance simulated time with
//! `ctx.time().sleep(..).await`. This one accumulates its own `Duration` from
//! the same seeded RNG instead, and its `Workload::run` contains no `.await` at
//! all. That is forced, not preferred: `moonpool_sim::Workload` is declared
//! `#[async_trait]` without `?Send`, so its future must be `Send`, while the H2
//! core is `Rc`-based by design (`Context` owns a `Box<dyn BufferSource>` over
//! a `Weak<RefCell<Pool>>`, and an `Rc<RefCell<L>>`; the worker runtime is
//! single-threaded per worker and
//! deliberately carries no `Arc<Mutex>` inside the event loop). Holding the
//! harness across a yield point therefore does not compile, and the only honest
//! alternatives are an `unsafe impl Send` lie or this.
//!
//! Nothing is lost. The workload is a single sequential task, so moonpool's
//! scheduler never had an interleaving to choose; what it supplies that matters
//! here is the seeded RNG and the per-seed campaign machinery, both of which are
//! untouched. The clock the CORE sees was never moonpool's either — it is
//! whatever this harness assigns to `Context::now` — and it advances only from
//! seeded draws, so it stays a pure function of the seed. The determinism guard
//! ([`h2_simulation_is_deterministic`]) checks that claim rather than assuming it.
//!
//! # Why the adapter's half of the timer is out of scope
//!
//! `Mux` — the `SessionState` adapter that owns the `TimeoutContainer` — samples
//! `Instant::now()` in `ready_inner` and `timeout_inner`, and `crate::timer`'s
//! `Timer::poll` derives its target tick from `Instant::now()` too
//! (`Timer::poll_to`, the injectable form, is private). Driving `Mux::timeout`
//! from here would be a host-clock read wearing a virtual clock's clothes —
//! exactly the vacuous-determinism trap. The adapter's re-validation is already
//! covered by `mux/mod.rs`'s own tests; what is NOT covered anywhere else, and
//! is what property 2 adds, is the core-side half under a clock the test owns.
//!
//! # Scenario grammar
//!
//! Each step draws one client action from a weighted grammar
//! ([`Action`]): open a stream (HEADERS, optionally END_STREAM), send DATA on an
//! open stream, RST_STREAM it, PING, WINDOW_UPDATE, a peer SETTINGS frame, a
//! SETTINGS ACK, or advance the virtual clock. Two orthogonal adversarial axes
//! ride on top and are redrawn every pump:
//!
//! - **read fragmentation** — `socket_read` hands back at most a seed-chosen
//!   number of bytes, so a 9-byte frame header routinely straddles two reads;
//! - **partial writes** — `socket_write` / `socket_write_vectored` accept at most
//!   a seed-chosen prefix and then report `WouldBlock`, exercising the
//!   edge-trigger re-arm discipline.
//!
//! A low-probability `buggify_with_prob!` arm additionally truncates one queued
//! client frame mid-payload, which the core must answer with a GOAWAY rather
//! than a panic.
//!
//! # Swarm configurations (Groce et al., "Swarm Testing", ISSTA 2012)
//!
//! Each seed draws a [`SwarmConfig`] from the seeded RNG BEFORE the first
//! action: a random subset of the OPTIONAL grammar features (50% inclusion
//! each), with the MANDATORY [`Action::OpenStream`] always retained and the
//! remaining weights renormalized. Omitting a SUPPRESSOR — [`Action::ResetStream`]
//! and [`Action::WindowUpdate`] each repair the very full-table / exhausted-window
//! state a ceiling bug needs — lets a seed drive the core into states the
//! all-features grammar repairs too eagerly. Seeds divisible by four keep the
//! inclusive all-features configuration (a bug needing `k` features together
//! appears in a coin-toss subset with probability `1/2^k`, so subsets complement
//! rather than replace it); campaign seeds are fixed to `0..n`, which reserves
//! exactly one inclusive run per four-seed cohort. Degenerate all-off and all-on
//! draws are repaired to non-empty proper subsets. The drawn configuration is a
//! pure function of the seed and is printed as one canonical `swarm-config` line
//! before the workload runs. `SOZU_SIM_SWARM=0` disables the draw entirely (zero
//! extra RNG consumption — byte-identical to the all-features grammar).
//!
//! # Replay / sweep ergonomics
//!
//! - `SOZU_H2_SIM_SEED=<u64|0xhex>` — replay that ONE seed verbosely.
//! - `SOZU_H2_SIM_SEEDS=<n>` — sweep `n` iterations (default 256).
//! - `SOZU_H2_SIM_STEPS=<n>` — client actions per seed (default 400).
//! - `SOZU_SIM_SWARM=0|1` — draw per-seed swarm configurations (default `1`).
//!
//! On a hard invariant violation the panic carries the failing seed (via
//! `current_sim_seed`) + step; moonpool also records it in the report's
//! `seeds_failing`.
//!
//! # Wire encoding is hand-rolled on purpose
//!
//! The frame encoder and decoder below are written from RFC 9113 §4/§6 and RFC
//! 7541 §6, NOT built on `sozu_lib::protocol::mux::parser` (which is public and
//! would have served). Feeding the core bytes produced by its own parser's
//! inverse, and then reading its answers back through that same parser, makes
//! the oracle a function of the code under test — the third of the four traps
//! [`metrics_lease_sim.rs`] names. [`tcp_preread_sim.rs`] hand-rolls its
//! ClientHello encoder for the same reason. The only values shared with `lib/`
//! are RFC-assigned wire constants (frame type bytes, flag bits, error codes),
//! which are properties of HTTP/2 rather than of Sōzu.
//!
//! [moonpool-sim]: https://crates.io/crates/moonpool-sim
//! [`udp_simulation.rs`]: ../../sim/tests/udp_simulation.rs
//! [`tcp_preread_sim.rs`]: ../../sim/tests/tcp_preread_sim.rs
//! [`metrics_lease_sim.rs`]: ../../sim/tests/metrics_lease_sim.rs
//! [`ConnectionH2`]: sozu_lib::protocol::mux::ConnectionH2
//! [`SocketHandler`]: sozu_lib::socket::SocketHandler
//! [`Context::now`]: sozu_lib::protocol::mux::Context::now

use std::{
    cell::RefCell,
    collections::{BTreeMap, BTreeSet, VecDeque},
    net::{SocketAddr, TcpListener as StdTcpListener},
    rc::Rc,
    sync::{Arc, Mutex},
    time::{Duration, Instant},
};

use async_trait::async_trait;
use moonpool_sim::{
    RandomProvider, SimContext, SimulationBuilder, SimulationReport, SimulationResult, Workload,
    buggify_with_prob, current_sim_seed,
};
use sozu_command_lib::{logging::CachedTags, ready::Ready};
use sozu_lib::{
    FrontendFromRequestError, L7ListenerHandler, ListenerHandler, Protocol, Readiness,
    pool::Pool,
    protocol::{
        http::{answers::HttpAnswers, parser::Method},
        mux::{
            Connection, ConnectionH2, Context, Endpoint, H2ConnectionConfig, H2FloodConfig,
            MuxResult, StreamState,
        },
    },
    router::RouteResult,
    socket::{SocketHandler, SocketResult, TransportProtocol},
    testing::Token,
};

// --------------------------------------------------------------------------
// RFC 9113 wire vocabulary, written from the RFC rather than imported.
// --------------------------------------------------------------------------

/// RFC 9113 §3.4. The 24-octet connection preface a client sends first.
const CLIENT_PREFACE: &[u8] = b"PRI * HTTP/2.0\r\n\r\nSM\r\n\r\n";
/// RFC 9113 §4.1. Every frame is prefixed by a 9-octet header.
const FRAME_HEADER_LEN: usize = 9;

const T_DATA: u8 = 0x0;
const T_HEADERS: u8 = 0x1;
const T_RST_STREAM: u8 = 0x3;
const T_SETTINGS: u8 = 0x4;
const T_PING: u8 = 0x6;
const T_GOAWAY: u8 = 0x7;
const T_WINDOW_UPDATE: u8 = 0x8;

const F_ACK: u8 = 0x1;
const F_END_STREAM: u8 = 0x1;
const F_END_HEADERS: u8 = 0x4;

/// RFC 9113 §7 error codes used by the oracles below.
const E_NO_ERROR: u32 = 0x0;
const E_REFUSED_STREAM: u32 = 0x7;

/// RFC 9113 §6.5.2 identifiers.
const S_INITIAL_WINDOW_SIZE: u16 = 0x4;
const S_MAX_CONCURRENT_STREAMS: u16 = 0x3;

/// RFC 9113 §6.5. A peer that does not acknowledge SETTINGS within the core's
/// `SETTINGS_ACK_TIMEOUT` is GOAWAY'd; the harness's client ACKs well inside it
/// so long scenarios are not drowned in `SettingsTimeout` noise.
const SETTINGS_ACK_BUDGET: Duration = Duration::from_secs(2);

// --------------------------------------------------------------------------
// Hand-rolled client-side frame encoder.
// --------------------------------------------------------------------------

/// RFC 9113 §4.1: 24-bit length, 8-bit type, 8-bit flags, 1 reserved bit +
/// 31-bit stream identifier, then the payload.
fn frame(ty: u8, flags: u8, stream_id: u32, payload: &[u8]) -> Vec<u8> {
    let len = payload.len();
    assert!(
        len < (1 << 24),
        "frame payload exceeds the 24-bit length field"
    );
    let mut out = Vec::with_capacity(FRAME_HEADER_LEN + len);
    out.push((len >> 16) as u8);
    out.push((len >> 8) as u8);
    out.push(len as u8);
    out.push(ty);
    out.push(flags);
    out.extend_from_slice(&(stream_id & 0x7fff_ffff).to_be_bytes());
    out.extend_from_slice(payload);
    out
}

fn settings(pairs: &[(u16, u32)]) -> Vec<u8> {
    let mut payload = Vec::with_capacity(pairs.len() * 6);
    for (id, value) in pairs {
        payload.extend_from_slice(&id.to_be_bytes());
        payload.extend_from_slice(&value.to_be_bytes());
    }
    frame(T_SETTINGS, 0, 0, &payload)
}

fn settings_ack() -> Vec<u8> {
    frame(T_SETTINGS, F_ACK, 0, &[])
}

fn ping(payload: u64) -> Vec<u8> {
    frame(T_PING, 0, 0, &payload.to_be_bytes())
}

fn rst_stream(stream_id: u32, error: u32) -> Vec<u8> {
    frame(T_RST_STREAM, 0, stream_id, &error.to_be_bytes())
}

fn window_update(stream_id: u32, increment: u32) -> Vec<u8> {
    frame(
        T_WINDOW_UPDATE,
        0,
        stream_id,
        &(increment & 0x7fff_ffff).to_be_bytes(),
    )
}

fn data(stream_id: u32, len: usize, end_stream: bool) -> Vec<u8> {
    // A fixed filler byte: the payload's CONTENT is irrelevant to every
    // property here and a varying one would only add trace noise.
    let payload = vec![b'x'; len];
    frame(
        T_DATA,
        if end_stream { F_END_STREAM } else { 0 },
        stream_id,
        &payload,
    )
}

/// RFC 7541 §6.1 / §6.2.2: a minimal, Huffman-free request header block.
///
/// `:method GET`, `:scheme http` and `:path /` are static-table entries 2, 6
/// and 4, encoded as one-byte indexed representations (`1` + 7-bit index).
/// `:authority` is static entry 1 with a literal value, encoded "without
/// indexing — indexed name" (`0000` + 4-bit index) so the encoder needs no
/// dynamic table of its own and the block is a pure function of its input.
fn request_header_block(authority: &str) -> Vec<u8> {
    let mut block = vec![0x82, 0x86, 0x84, 0x01];
    let value = authority.as_bytes();
    assert!(
        value.len() < 127,
        "authority value needs no 7-bit continuation"
    );
    block.push(value.len() as u8);
    block.extend_from_slice(value);
    block
}

fn headers(stream_id: u32, authority: &str, end_stream: bool) -> Vec<u8> {
    let flags = F_END_HEADERS | if end_stream { F_END_STREAM } else { 0 };
    frame(
        T_HEADERS,
        flags,
        stream_id,
        &request_header_block(authority),
    )
}

// --------------------------------------------------------------------------
// Hand-rolled server-side frame decoder (RFC 9113 §4.1, read direction).
// --------------------------------------------------------------------------

#[derive(Debug, Clone, PartialEq, Eq)]
struct WireFrame {
    ty: u8,
    flags: u8,
    stream_id: u32,
    payload: Vec<u8>,
}

impl WireFrame {
    fn u32_at(&self, offset: usize) -> Option<u32> {
        let bytes = self.payload.get(offset..offset + 4)?;
        Some(u32::from_be_bytes([bytes[0], bytes[1], bytes[2], bytes[3]]))
    }

    /// RFC 9113 §6.4: the RST_STREAM error code.
    fn rst_error(&self) -> Option<u32> {
        (self.ty == T_RST_STREAM).then(|| self.u32_at(0)).flatten()
    }

    /// RFC 9113 §6.8: the GOAWAY error code (after the 4-octet last-stream-id).
    fn goaway_error(&self) -> Option<u32> {
        (self.ty == T_GOAWAY).then(|| self.u32_at(4)).flatten()
    }

    /// RFC 9113 §6.9: the WINDOW_UPDATE increment, reserved bit masked off.
    fn window_increment(&self) -> Option<u32> {
        (self.ty == T_WINDOW_UPDATE)
            .then(|| self.u32_at(0))
            .flatten()
            .map(|v| v & 0x7fff_ffff)
    }

    /// A short, stable rendering for the trace. Payload bytes are folded to a
    /// digest rather than printed: the properties are about frame identity and
    /// ordering, and a raw dump would make the trace unreadable without making
    /// it more sensitive.
    fn render(&self) -> String {
        let extra = match self.ty {
            T_RST_STREAM => format!(":e{}", self.rst_error().unwrap_or(u32::MAX)),
            T_GOAWAY => format!(
                ":last{}:e{}",
                self.u32_at(0).unwrap_or(u32::MAX) & 0x7fff_ffff,
                self.goaway_error().unwrap_or(u32::MAX)
            ),
            T_WINDOW_UPDATE => format!(":i{}", self.window_increment().unwrap_or(u32::MAX)),
            _ => String::new(),
        };
        format!(
            "f:{}:{}:{}:{}{}:{:x}",
            self.ty,
            self.flags,
            self.stream_id,
            self.payload.len(),
            extra,
            fnv1a(&self.payload)
        )
    }
}

/// Decode as many whole frames as `bytes` holds, returning them plus the number
/// of bytes consumed. A trailing partial frame is left for the next call.
fn decode_frames(bytes: &[u8]) -> (Vec<WireFrame>, usize) {
    let mut frames = Vec::new();
    let mut offset = 0usize;
    while let Some(header) = bytes.get(offset..offset + FRAME_HEADER_LEN) {
        let len = ((header[0] as usize) << 16) | ((header[1] as usize) << 8) | (header[2] as usize);
        let body_start = offset + FRAME_HEADER_LEN;
        let Some(payload) = bytes.get(body_start..body_start + len) else {
            break;
        };
        frames.push(WireFrame {
            ty: header[3],
            flags: header[4],
            stream_id: u32::from_be_bytes([header[5], header[6], header[7], header[8]])
                & 0x7fff_ffff,
            payload: payload.to_vec(),
        });
        offset = body_start + len;
    }
    (frames, offset)
}

fn fnv1a(bytes: &[u8]) -> u64 {
    let mut hash: u64 = 0xcbf2_9ce4_8422_2325;
    for &b in bytes {
        hash ^= b as u64;
        hash = hash.wrapping_mul(0x0000_0100_0000_01b3);
    }
    hash
}

// --------------------------------------------------------------------------
// The in-memory socket. Q10 option (a): the core keeps its `Front` type
// parameter, so the harness supplies the byte boundary itself.
// --------------------------------------------------------------------------

/// A `SocketHandler` whose two directions are plain in-memory queues.
///
/// The `placeholder` field is dead weight the trait still demands:
/// `SocketHandler::socket_ref`/`socket_mut` return `&mio::net::TcpStream`, a
/// concrete OS type no in-memory transport can synthesise. It is a connected
/// loopback stream that is never read, never written and never registered —
/// exactly what `mux/mod.rs`'s own `test_support::connected_socket` keeps for
/// the same reason. `peer_addr` deliberately answers a FIXED address rather
/// than the placeholder's OS-assigned one, so no ephemeral port can reach the
/// trace. Both disappear when `Front` does.
struct SimSocket {
    inbound: VecDeque<u8>,
    outbound: Vec<u8>,
    /// Peer sent its last byte; further reads report `Closed`, not `WouldBlock`.
    peer_shutdown: bool,
    /// Adversarial read fragmentation: at most this many bytes per
    /// `socket_read`, redrawn per pump so frame headers straddle reads.
    read_chunk: usize,
    /// Adversarial partial writes: at most this many bytes per `socket_write`,
    /// after which the socket reports `WouldBlock`.
    write_chunk: usize,
    placeholder: mio::net::TcpStream,
}

/// The accepted peer of every [`SimSocket`] placeholder. Held for the harness's
/// lifetime so the loopback stream stays connected, and never otherwise used —
/// dropping it would close the peer end under a socket the core may still ask
/// for a reference to.
struct PlaceholderPeer {
    _keepalive: std::net::TcpStream,
}

impl SimSocket {
    fn new() -> (Self, PlaceholderPeer) {
        let listener = StdTcpListener::bind("127.0.0.1:0").expect("bind placeholder listener");
        let address = listener.local_addr().expect("placeholder local addr");
        let placeholder = mio::net::TcpStream::connect(address).expect("connect placeholder");
        let (peer, _) = listener.accept().expect("accept placeholder peer");
        peer.set_nonblocking(true).expect("placeholder nonblocking");
        (
            SimSocket {
                inbound: VecDeque::new(),
                outbound: Vec::new(),
                peer_shutdown: false,
                read_chunk: usize::MAX,
                write_chunk: usize::MAX,
                placeholder,
            },
            PlaceholderPeer { _keepalive: peer },
        )
    }
}

impl std::fmt::Debug for SimSocket {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        // Deliberately prints neither address nor buffer contents: this type's
        // `Debug` reaches the core's own `Debug` impl and from there its log
        // lines, and an ephemeral port there would be a nondeterminism leak.
        f.debug_struct("SimSocket")
            .field("inbound", &self.inbound.len())
            .field("outbound", &self.outbound.len())
            .field("peer_shutdown", &self.peer_shutdown)
            .finish()
    }
}

impl SocketHandler for SimSocket {
    fn socket_read(&mut self, buf: &mut [u8]) -> (usize, SocketResult) {
        let want = buf.len().min(self.read_chunk);
        let mut written = 0usize;
        while written < want {
            let Some(byte) = self.inbound.pop_front() else {
                break;
            };
            buf[written] = byte;
            written += 1;
        }
        if written > 0 {
            return (written, SocketResult::Continue);
        }
        if self.peer_shutdown && self.inbound.is_empty() {
            (0, SocketResult::Closed)
        } else {
            (0, SocketResult::WouldBlock)
        }
    }

    fn socket_write(&mut self, buf: &[u8]) -> (usize, SocketResult) {
        let take = buf.len().min(self.write_chunk);
        self.outbound.extend_from_slice(&buf[..take]);
        if take == buf.len() {
            (take, SocketResult::Continue)
        } else {
            (take, SocketResult::WouldBlock)
        }
    }

    fn socket_write_vectored(&mut self, slices: &[std::io::IoSlice]) -> (usize, SocketResult) {
        let mut budget = self.write_chunk;
        let mut written = 0usize;
        for slice in slices {
            if budget == 0 {
                return (written, SocketResult::WouldBlock);
            }
            let take = slice.len().min(budget);
            self.outbound.extend_from_slice(&slice[..take]);
            written += take;
            budget -= take;
            if take < slice.len() {
                return (written, SocketResult::WouldBlock);
            }
        }
        (written, SocketResult::Continue)
    }

    fn socket_close(&mut self) {}

    fn socket_ref(&self) -> &mio::net::TcpStream {
        &self.placeholder
    }

    fn socket_mut(&mut self) -> &mut mio::net::TcpStream {
        &mut self.placeholder
    }

    fn peer_addr(&self) -> Option<SocketAddr> {
        // Fixed, never the placeholder's OS-assigned port — see the type doc.
        Some(SIM_PEER_ADDRESS.parse().expect("fixed peer address parses"))
    }

    fn protocol(&self) -> TransportProtocol {
        TransportProtocol::Tcp
    }

    fn read_error(&self) {}

    fn write_error(&self) {}
}

const SIM_PEER_ADDRESS: &str = "10.0.0.1:44444";
const SIM_PUBLIC_ADDRESS: &str = "127.0.0.1:8080";

// --------------------------------------------------------------------------
// Listener. Implements exactly the required methods of `ListenerHandler` +
// `L7ListenerHandler`; every other knob keeps its trait default.
// --------------------------------------------------------------------------

struct SimListener {
    address: SocketAddr,
    answers: Rc<RefCell<HttpAnswers>>,
    connection_config: H2ConnectionConfig,
    flood_config: H2FloodConfig,
}

impl SimListener {
    fn new(connection_config: H2ConnectionConfig) -> Self {
        SimListener {
            address: SIM_PUBLIC_ADDRESS.parse().expect("listener address parses"),
            answers: Rc::new(RefCell::new(
                HttpAnswers::new(&BTreeMap::new()).expect("default answers build"),
            )),
            connection_config,
            flood_config: H2FloodConfig::default(),
        }
    }
}

impl ListenerHandler for SimListener {
    fn get_addr(&self) -> &SocketAddr {
        &self.address
    }
    fn get_tags(&self, _key: &str) -> Option<&CachedTags> {
        None
    }
    fn set_tags(&mut self, _key: String, _tags: Option<BTreeMap<String, String>>) {}
    fn protocol(&self) -> Protocol {
        Protocol::HTTP
    }
    fn public_address(&self) -> SocketAddr {
        self.address
    }
}

impl L7ListenerHandler for SimListener {
    fn get_sticky_name(&self) -> &str {
        "SOZUBALANCEID"
    }
    fn get_connect_timeout(&self) -> u32 {
        10
    }
    fn frontend_from_request(
        &self,
        _host: &str,
        _uri: &str,
        _method: &Method,
    ) -> Result<RouteResult, FrontendFromRequestError> {
        // Routing lives in `Router::connect`, which only `Mux::ready_inner`
        // drives — never `ConnectionH2::readable`. Nothing in this harness can
        // reach this method; it answers an error so a future caller fails
        // loudly instead of silently routing somewhere.
        Err(FrontendFromRequestError::InvalidCharsAfterHost(
            "the H2 simulation routes nothing".to_owned(),
        ))
    }
    fn get_answers(&self) -> &Rc<RefCell<HttpAnswers>> {
        &self.answers
    }
    fn get_h2_flood_config(&self) -> H2FloodConfig {
        self.flood_config
    }
    fn get_h2_connection_config(&self) -> H2ConnectionConfig {
        self.connection_config
    }
}

// --------------------------------------------------------------------------
// Endpoint. The backend side of the core's callbacks, recorded rather than
// performed: no backend exists in this simulation.
// --------------------------------------------------------------------------

/// `mux`'s own name for a stream index is the private alias
/// `GlobalStreamId = usize`; the public trait signatures spell it out as
/// `usize`, which is what this harness has to write.
#[derive(Debug, Default)]
struct SimEndpointState {
    readiness: Readiness,
    started: Vec<usize>,
    ended: Vec<usize>,
}

/// Mirrors `mux/connection.rs`'s `EndpointServer`/`EndpointClient` shape: a
/// newtype over a mutable borrow, so `readable`/`writable` can take it by value
/// while the harness keeps owning the state.
#[derive(Debug)]
struct SimEndpoint<'a>(&'a mut SimEndpointState);

impl Endpoint for SimEndpoint<'_> {
    fn readiness(&self, _token: Token) -> &Readiness {
        &self.0.readiness
    }

    fn readiness_mut(&mut self, _token: Token) -> &mut Readiness {
        &mut self.0.readiness
    }

    fn peer_rtt(&self, _token: Token) -> Option<Duration> {
        // A live-socket property on the embedder's side of the boundary; the
        // simulation has no socket to sample. `None` is the documented answer
        // for a token that does not resolve.
        None
    }

    fn end_stream<L: ListenerHandler + L7ListenerHandler>(
        &mut self,
        _token: Token,
        stream: usize,
        _context: &mut Context<L>,
    ) {
        self.0.ended.push(stream);
    }

    fn start_stream<L: ListenerHandler + L7ListenerHandler>(
        &mut self,
        _token: Token,
        stream: usize,
        _context: &mut Context<L>,
    ) -> bool {
        self.0.started.push(stream);
        // No backend exists, so no stream can be attached to one. This is the
        // `Position::Server` frontend's own endpoint: the core uses it to offer
        // a stream to the peer side, and a refusal is the honest answer.
        false
    }
}

// --------------------------------------------------------------------------
// The harness.
// --------------------------------------------------------------------------

/// Hard ceiling on one `pump`'s readable/writable alternation. `Mux::ready_inner`
/// carries the same kind of budget (`MAX_LOOP_ITERATIONS`); reaching it here is a
/// harness failure, never a quiet break. Generous because the harness's most
/// adversarial schedule moves one byte per `socket_read` and one per
/// `socket_write`, so a single frame legitimately costs a dozen iterations.
const MAX_PUMP_ITERATIONS: usize = 200_000;

struct H2Harness {
    connection: Connection<SimSocket>,
    context: Context<SimListener>,
    endpoint: SimEndpointState,
    /// Deadline published at construction, before any pass. Every deadline in
    /// the trace is rendered relative to it, so the one construction-time
    /// `Instant::now()` inside `ConnectionH2::new` never reaches the trace.
    deadline_base: Option<Instant>,
    /// Instant the connection was accepted at; every `t` in the trace is
    /// relative to it.
    clock_base: Option<Instant>,
    /// Bytes the core has written and the harness has not yet decoded.
    pending_out: Vec<u8>,
    /// Every frame the core has emitted, in order.
    emitted: Vec<WireFrame>,
    trace: Vec<String>,
    _pool: Rc<RefCell<Pool>>,
    _placeholder_peer: PlaceholderPeer,
}

impl H2Harness {
    /// `clock_base` is the instant the simulated connection is considered to
    /// have been accepted at. It is not decoration: `ConnectionH2::new` takes
    /// the one wall-clock sample the core still makes outside `Mux`'s sampling
    /// points, and stamps the connection deadline with it. Left alone, every
    /// deadline this harness reports would carry the microseconds between that
    /// read and the harness's own — enough to flip a millisecond boundary and
    /// fork the trace between two runs of the same seed. Re-arming through the
    /// public `Connection::set_timeout_duration` at `clock_base` replaces that
    /// anchor with one the harness owns, which is what makes the determinism
    /// guard a real check rather than a coin toss.
    fn new(
        connection_config: H2ConnectionConfig,
        timeouts: HarnessTimeouts,
        clock_base: Instant,
    ) -> Self {
        // Two checkouts per stream plus the H2 connection's own `zero` buffer.
        // 512 is "effectively unbounded" for every scenario that is not about
        // exhaustion; the one that is passes its own ceiling.
        Self::with_pool_maximum(connection_config, timeouts, clock_base, 512)
    }

    /// [`Self::new`] with an explicit buffer-pool ceiling.
    ///
    /// The pool maximum is the *only* knob that makes checkout failure
    /// reachable from outside the core: both H2 checkout sites —
    /// `ConnectionH2::new` for `zero`, and `Stream::new` for a stream's
    /// request/response pair — go through `Pool::checkout`, which answers
    /// `None` once `used` has reached `maximum_capacity`. Sizing the pool is
    /// therefore how this harness forces exhaustion deterministically today,
    /// without an injected allocator and without a timing race.
    fn with_pool_maximum(
        connection_config: H2ConnectionConfig,
        timeouts: HarnessTimeouts,
        clock_base: Instant,
        pool_maximum: usize,
    ) -> Self {
        // 16393 is the documented floor for an H2-enabled buffer pool — the
        // 16384-octet maximum frame payload plus its 9-octet header — and a
        // smaller one deadlocks the mux on a full-size frame. Nothing here
        // sends one today, so the value is a guard against a future scenario
        // that does rather than a fix for a present symptom.
        const H2_MIN_BUFFER_SIZE: usize = 16_384 + FRAME_HEADER_LEN;
        let pool = Rc::new(RefCell::new(Pool::with_capacity(
            1,
            pool_maximum,
            H2_MIN_BUFFER_SIZE,
        )));
        let (socket, placeholder_peer) = SimSocket::new();
        let listener = Rc::new(RefCell::new(SimListener::new(connection_config)));
        let mut context = Context::new(
            // `Ulid: From<u128>` in argument position — never `Ulid::generate()`,
            // which reads the wall clock and a random source.
            0u128.into(),
            Rc::downgrade(&pool),
            listener,
            Some(SIM_PEER_ADDRESS.parse().expect("peer address parses")),
            SIM_PUBLIC_ADDRESS.parse().expect("public address parses"),
        );
        let connection = Connection::new_h2_server(
            0u128.into(),
            socket,
            // The same source the session's streams draw from, so a pool
            // ceiling bounds the connection and its streams together — which
            // is what makes exhaustion reachable by sizing one number.
            &mut *context.buffers,
            timeouts.connection,
            H2FloodConfig::default(),
            connection_config,
            timeouts.stream_idle,
            None,
        )
        .expect("the buffer pool must yield the H2 connection buffer");
        let mut connection = connection;
        connection.set_timeout_duration(timeouts.connection, clock_base);
        let deadline_base = connection.poll_timeout();
        debug_assert_eq!(
            deadline_base,
            clock_base.checked_add(timeouts.connection),
            "re-arming must put the deadline exactly one timeout past the injected base"
        );
        H2Harness {
            connection,
            context,
            endpoint: SimEndpointState::default(),
            deadline_base,
            clock_base: Some(clock_base),
            pending_out: Vec::new(),
            emitted: Vec::new(),
            trace: Vec::new(),
            _pool: pool,
            _placeholder_peer: placeholder_peer,
        }
    }

    fn h2(&mut self) -> &mut ConnectionH2<SimSocket> {
        match &mut self.connection {
            Connection::H2(h2) => h2,
            Connection::H1(_) => unreachable!("new_h2_server built an H2 connection"),
        }
    }

    fn socket_mut(&mut self) -> &mut SimSocket {
        &mut self.h2().socket
    }

    /// Queue client bytes for the core to read, and arm the readable event the
    /// event loop would have delivered.
    fn feed(&mut self, bytes: &[u8]) {
        self.socket_mut().inbound.extend(bytes.iter().copied());
        self.connection.readiness_mut().event |= Ready::READABLE;
    }

    /// Feed the RFC 9113 §3.4 preface plus the client's SETTINGS and let the
    /// core consume them, in one piece.
    ///
    /// The read-fragmentation axis starts after this call. That is a SCOPING
    /// choice, not a workaround: the handshake is connection setup and none of
    /// the properties below is about it, so spending the fragmentation budget
    /// there would buy coverage of a path that `ConnectionH2`'s own
    /// `client_preface_short_read_*` regression sweep already owns. Feeding it
    /// whole also keeps every scenario's first pass identical, which is one
    /// fewer thing for the determinism guard to have to explain.
    ///
    /// It was briefly a workaround. Writing this harness surfaced a real defect
    /// here — `ConnectionH2::handle_read`'s early-preface guard compared the
    /// WHOLE accumulated window against the 24-octet magic string with
    /// `starts_with`, while the window it is handed is `CLIENT_PREFACE_SIZE`,
    /// the magic string plus a 9-octet SETTINGS header, so a short read landing
    /// past 24 octets force-disconnected a conforming client. That is fixed:
    /// the guard now compares only `&i[..i.len().min(H2_PRI.len())]`, deriving
    /// the width from `serializer::H2_PRI` so the two cannot drift
    /// (sozu-proxy/sozu#1487, fixed in `fa0b93a4`). Its regression coverage
    /// sweeps the chunk size rather than sampling it and pins a failing band
    /// wider than the one measured here — 1..=10, 13..=16 and 25..=32.
    ///
    /// Extending the fragmentation axis over the handshake is therefore now
    /// possible — measured from this harness against `9472cd96`, every
    /// `socket_read` chunk size in 1..=40 completes the handshake and opens a
    /// stream, where before the fix 1, 25, 26 and 32 did not — and is
    /// deliberately left to its own changeset. Nothing else in this file is
    /// carved out: every byte after the handshake goes through the axis.
    fn bootstrap(&mut self, at: Instant, peer_initial_window: u32) {
        self.feed(&client_preface(peer_initial_window));
        self.pump(at, usize::MAX, usize::MAX);
    }

    /// Deliver a timer callback at `at`, then let the core write whatever it
    /// queued in response.
    ///
    /// `ConnectionH2::cancel_timed_out_streams` is the core's public timeout
    /// entry point — the one `Mux::timeout` reaches without going through
    /// `readable`, and the one a wheel delivery lands on. Driving it directly is
    /// what makes "the wheel fired early" expressible here: the adapter half
    /// that decides WHETHER a delivery is due is wall-clock (see the module
    /// doc), but the core half that acts on one takes its instant from
    /// `Context::now`, which this harness owns.
    fn timeout_pass(&mut self, at: Instant) -> MuxResult {
        self.context.now = at;
        {
            let Self {
                connection,
                context,
                endpoint,
                ..
            } = self;
            let Connection::H2(h2) = connection else {
                unreachable!("new_h2_server built an H2 connection")
            };
            h2.cancel_timed_out_streams(context, &mut SimEndpoint(endpoint));
        }
        self.pump(at, usize::MAX, usize::MAX)
    }

    fn shutdown_peer(&mut self) {
        self.socket_mut().peer_shutdown = true;
        self.connection.readiness_mut().event |= Ready::READABLE;
    }

    /// Drive the core the way `Mux::ready_inner` does: refresh the clock
    /// snapshot, then alternate `readable`/`writable` while the filtered
    /// readiness asks for them.
    ///
    /// Returns the last `MuxResult`. `fragmentation` and `write_budget` are the
    /// two adversarial axes; `usize::MAX` on either disables it.
    fn pump(&mut self, now: Instant, fragmentation: usize, write_budget: usize) -> MuxResult {
        {
            let socket = self.socket_mut();
            socket.read_chunk = fragmentation.max(1);
            socket.write_chunk = write_budget.max(1);
        }
        let mut result = MuxResult::Continue;
        let mut iterations = 0usize;
        let mut idle_rounds = 0usize;
        loop {
            self.context.now = now;
            // Re-arm the writable event while the core still holds queued bytes.
            //
            // This is what the event loop does for real: `socket_write`
            // returning `WouldBlock` clears the core's WRITABLE bit, the kernel
            // socket buffer drains, and epoll signals writable again. Without
            // it the harness's own partial-write axis would be indistinguishable
            // from a peer that never reads — a scenario in which NOTHING is
            // observable, so every assertion below would pass vacuously.
            if self.has_pending_write() {
                self.connection.readiness_mut().event |= Ready::WRITABLE;
            }
            // Re-arm the readable event while the peer's bytes are still queued.
            //
            // The core clears its own READABLE bit whenever a pass ends with
            // nothing owed (`ConnectionH2::poll_read_target`'s idle
            // `expect_read` branch) and waits for the next epoll wake-up. A real
            // peer that has written more produces exactly that wake-up; this
            // harness's `feed` is the same statement, and several `feed` calls
            // before one `pump` are one peer write as far as the core can tell.
            // Without the re-arm the harness silently under-delivers: bytes sit
            // in its own queue and every assertion about what the core did with
            // them becomes vacuous.
            //
            // It cannot mask a stuck core. Progress is measured in octets moved,
            // so a core that answers a re-armed event by reading nothing leaves
            // `progressed` false and the loop exits on the very next check.
            if self.inbound_len() > 0 {
                self.connection.readiness_mut().event |= Ready::READABLE;
            }
            let interest = self.connection.readiness().filter_interest();
            let mut progressed = false;

            if interest.is_readable() {
                let before = self.outbound_len();
                let inbound_before = self.inbound_len();
                result = {
                    let Self {
                        connection,
                        context,
                        endpoint,
                        ..
                    } = self;
                    let Connection::H2(h2) = connection else {
                        unreachable!("new_h2_server built an H2 connection")
                    };
                    h2.readable(context, SimEndpoint(endpoint))
                };
                progressed |= self.outbound_len() != before || self.inbound_len() != inbound_before;
                if !matches!(result, MuxResult::Continue) {
                    break;
                }
            }

            let interest = self.connection.readiness().filter_interest();
            if interest.is_writable() {
                let before = self.outbound_len();
                result = {
                    let Self {
                        connection,
                        context,
                        endpoint,
                        ..
                    } = self;
                    let Connection::H2(h2) = connection else {
                        unreachable!("new_h2_server built an H2 connection")
                    };
                    h2.writable(context, SimEndpoint(endpoint))
                };
                progressed |= self.outbound_len() != before;
                if !matches!(result, MuxResult::Continue) {
                    break;
                }
            }

            // A pass that moved no octets is not necessarily a settled
            // connection: dispatching a zero-length frame — a SETTINGS ACK, an
            // empty DATA, a PING ACK being queued — advances the core's state
            // without touching either queue. Breaking on the first such pass
            // stops the harness one frame short of everything that follows it,
            // silently. Allow a small run of them and let the octet-moving pass
            // that comes after reset the count.
            const IDLE_ROUNDS_BEFORE_SETTLED: usize = 4;
            if progressed {
                idle_rounds = 0;
            } else {
                idle_rounds += 1;
                if idle_rounds >= IDLE_ROUNDS_BEFORE_SETTLED {
                    break;
                }
            }
            iterations += 1;
            assert!(
                iterations < MAX_PUMP_ITERATIONS,
                "pump made progress for {MAX_PUMP_ITERATIONS} iterations without settling",
            );
        }

        self.harvest();
        self.record(now, result);
        result
    }

    fn has_pending_write(&self) -> bool {
        match &self.connection {
            Connection::H2(h2) => h2.has_pending_write(),
            Connection::H1(_) => unreachable!("new_h2_server built an H2 connection"),
        }
    }

    fn outbound_len(&mut self) -> usize {
        self.socket_mut().outbound.len()
    }

    /// Octets the harness has queued that the core has not read yet. Zero is
    /// what makes a "the peer spent N octets" oracle a statement about what the
    /// core RECEIVED rather than about what the harness queued.
    fn inbound_len(&mut self) -> usize {
        self.socket_mut().inbound.len()
    }

    /// Move whatever the core wrote into the decoded frame history.
    fn harvest(&mut self) {
        let written = std::mem::take(&mut self.socket_mut().outbound);
        self.pending_out.extend_from_slice(&written);
        let (frames, consumed) = decode_frames(&self.pending_out);
        self.pending_out.drain(..consumed);
        self.emitted.extend(frames);
    }

    /// Append this pass's observable state to the trace.
    ///
    /// Everything here is behaviour the peer or the embedder can see: frames
    /// on the wire, the live stream count, the published deadline, the result.
    /// Nothing reads a private field, so the trace survives the byte-in /
    /// byte-out extraction unchanged.
    fn record(&mut self, now: Instant, result: MuxResult) {
        let already = self
            .trace
            .iter()
            .filter(|line| line.starts_with("f:"))
            .count();
        let new_frames: Vec<String> = self.emitted[already..].iter().map(|f| f.render()).collect();
        self.trace.extend(new_frames);

        let deadline = self.relative_deadline();
        let streams: Vec<String> = self
            .context
            .streams
            .iter()
            .enumerate()
            .map(|(id, stream)| format!("{id}={}", state_tag(&stream.state)))
            .collect();
        let live = self.h2().stream_count();
        let clock = now.saturating_duration_since(self.clock_base()).as_millis();
        self.trace.push(format!(
            "p:t{clock}:r{}:n{live}:d{}:[{}]",
            result_tag(result),
            deadline
                .map(|d| d.to_string())
                .unwrap_or_else(|| "-".to_owned()),
            streams.join(","),
        ));
    }

    /// The instant the connection was accepted at. Every `t` in the trace is
    /// relative to it, so no absolute `Instant` ever reaches the fingerprint.
    fn clock_base(&self) -> Instant {
        self.clock_base.unwrap_or(self.context.now)
    }

    /// The core's published deadline in milliseconds relative to the deadline
    /// it published at construction. `None` when the core wants no timer.
    fn relative_deadline(&self) -> Option<i128> {
        let base = self.deadline_base?;
        let current = self.connection.poll_timeout()?;
        Some(if current >= base {
            current.duration_since(base).as_millis() as i128
        } else {
            -(base.duration_since(current).as_millis() as i128)
        })
    }

    fn fingerprint(&self) -> u64 {
        fnv1a(self.trace.join("\n").as_bytes())
    }

    fn emitted_of(&self, ty: u8) -> impl Iterator<Item = &WireFrame> {
        self.emitted.iter().filter(move |f| f.ty == ty)
    }

    fn goaway_seen(&self) -> bool {
        self.emitted.iter().any(|f| f.ty == T_GOAWAY)
    }
}

#[derive(Debug, Clone, Copy)]
struct HarnessTimeouts {
    connection: Duration,
    stream_idle: Duration,
}

impl Default for HarnessTimeouts {
    fn default() -> Self {
        HarnessTimeouts {
            connection: Duration::from_secs(60),
            stream_idle: Duration::from_secs(30),
        }
    }
}

fn state_tag(state: &StreamState) -> &'static str {
    match state {
        StreamState::Idle => "I",
        StreamState::Link => "L",
        StreamState::Linked(_) => "K",
        StreamState::Unlinked => "U",
        StreamState::Recycle => "R",
    }
}

fn result_tag(result: MuxResult) -> &'static str {
    match result {
        MuxResult::Continue => "C",
        MuxResult::Upgrade => "U",
        MuxResult::CloseSession => "X",
    }
}

/// The preface every scenario opens with: RFC 9113 §3.4 magic, the client's own
/// SETTINGS, and an immediate ACK budget for the core's.
fn client_preface(initial_window: u32) -> Vec<u8> {
    let mut bytes = CLIENT_PREFACE.to_vec();
    bytes.extend_from_slice(&settings(&[
        (S_INITIAL_WINDOW_SIZE, initial_window),
        (S_MAX_CONCURRENT_STREAMS, 128),
    ]));
    bytes
}

// --------------------------------------------------------------------------
// Scenario grammar + swarm configuration.
// --------------------------------------------------------------------------

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Action {
    /// MANDATORY: without it nothing else has a stream to act on.
    OpenStream,
    SendData,
    /// SUPPRESSOR: frees a concurrent-stream slot the ceiling test needs full.
    ResetStream,
    /// SUPPRESSOR: refills the connection window the flow-control test needs empty.
    WindowUpdate,
    Ping,
    PeerSettings,
    SettingsAck,
    AdvanceClock,
}

const ACTIONS: [Action; 8] = [
    Action::OpenStream,
    Action::SendData,
    Action::ResetStream,
    Action::WindowUpdate,
    Action::Ping,
    Action::PeerSettings,
    Action::SettingsAck,
    Action::AdvanceClock,
];

/// Relative draw weights over [`ACTIONS`], renormalized across whatever the
/// swarm configuration leaves enabled.
const WEIGHTS: [u32; 8] = [30, 30, 12, 8, 5, 3, 7, 15];

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct SwarmConfig {
    enabled: [bool; 8],
}

impl SwarmConfig {
    fn full() -> Self {
        SwarmConfig { enabled: [true; 8] }
    }

    fn is_enabled(&self, action: Action) -> bool {
        self.enabled[ACTIONS
            .iter()
            .position(|a| *a == action)
            .expect("known action")]
    }

    /// Draw this seed's configuration. Seeds divisible by four keep the
    /// inclusive configuration; every other seed takes a coin-toss subset,
    /// repaired away from the two degenerate draws.
    fn draw(ctx: &SimContext, seed: u64) -> Self {
        if inclusive_seed(seed) {
            return SwarmConfig::full();
        }
        let mut enabled = [false; 8];
        for slot in enabled.iter_mut() {
            *slot = ctx.random().random_range(0..2u8) == 1;
        }
        // `OpenStream` is MANDATORY: every other feature needs a stream.
        enabled[0] = true;
        if enabled.iter().all(|e| *e) {
            // An all-on draw is the inclusive configuration, which the reserved
            // seeds already cover; turn the least disruptive feature off so the
            // subset stays proper.
            enabled[4] = false;
        }
        SwarmConfig { enabled }
    }

    fn log_line(&self, seed: u64, swarm: bool) -> String {
        let names: Vec<&str> = ACTIONS
            .iter()
            .zip(self.enabled.iter())
            .filter(|(_, on)| **on)
            .map(|(action, _)| action_name(*action))
            .collect();
        format!(
            "swarm-config seed={seed:#x} swarm={swarm} inclusive={} features=[{}]",
            self.enabled.iter().all(|e| *e),
            names.join(",")
        )
    }

    fn pick(&self, ctx: &SimContext) -> Action {
        let total: u32 = ACTIONS
            .iter()
            .zip(self.enabled.iter())
            .filter(|(_, on)| **on)
            .map(|(action, _)| weight_of(*action))
            .sum();
        let mut draw = ctx.random().random_range(0..total);
        for (action, on) in ACTIONS.iter().zip(self.enabled.iter()) {
            if !*on {
                continue;
            }
            let w = weight_of(*action);
            if draw < w {
                return *action;
            }
            draw -= w;
        }
        Action::OpenStream
    }
}

fn weight_of(action: Action) -> u32 {
    WEIGHTS[ACTIONS
        .iter()
        .position(|a| *a == action)
        .expect("known action")]
}

fn action_name(action: Action) -> &'static str {
    match action {
        Action::OpenStream => "OpenStream",
        Action::SendData => "SendData",
        Action::ResetStream => "ResetStream",
        Action::WindowUpdate => "WindowUpdate",
        Action::Ping => "Ping",
        Action::PeerSettings => "PeerSettings",
        Action::SettingsAck => "SettingsAck",
        Action::AdvanceClock => "AdvanceClock",
    }
}

// --------------------------------------------------------------------------
// Shadow model. Counts only what the HARNESS itself did, never what the core
// reported, so every bound below is checked against an independent oracle.
// --------------------------------------------------------------------------

#[derive(Debug, Default)]
struct Model {
    /// Wire ids the harness has opened with HEADERS, in order.
    opened: Vec<u32>,
    /// Wire ids the harness has RST_STREAM'd or half-closed with END_STREAM.
    finished: Vec<u32>,
    /// Total DATA payload octets the harness has put on the wire.
    data_sent: u64,
    /// Per-stream DATA payload octets the harness sent WITHOUT `END_STREAM`.
    /// `ConnectionH2::handle_data_frame` deliberately skips the per-stream
    /// WINDOW_UPDATE on an `END_STREAM` frame (the stream is half-closed and
    /// its window is moot), so those octets are excluded here rather than
    /// subtracted later.
    data_sent_per_stream: BTreeMap<u32, u64>,
    /// Total connection-level (stream 0) WINDOW_UPDATE octets the harness granted.
    window_granted: u64,
    next_stream_id: u32,
    settings_acked_at: Option<Duration>,
    peak_open: usize,
}

impl Model {
    fn allocate_stream(&mut self) -> u32 {
        // RFC 9113 §5.1.1: client-initiated streams are odd and monotonic.
        let id = self.next_stream_id;
        self.next_stream_id += 2;
        self.opened.push(id);
        self.peak_open = self.peak_open.max(self.open_ids().count());
        id
    }

    fn open_ids(&self) -> impl Iterator<Item = u32> + '_ {
        self.opened
            .iter()
            .copied()
            .filter(move |id| !self.finished.contains(id))
    }

    fn finish(&mut self, id: u32) {
        if !self.finished.contains(&id) {
            self.finished.push(id);
        }
    }
}

// --------------------------------------------------------------------------
// Workload.
// --------------------------------------------------------------------------

type FingerprintSink = Arc<Mutex<Vec<(u64, usize, usize)>>>;
type ConfigSink = Arc<Mutex<Vec<SwarmConfig>>>;

struct H2SimWorkload {
    steps: usize,
    verbose: bool,
    sink: Option<FingerprintSink>,
    swarm: bool,
    config_sink: Option<ConfigSink>,
}

#[async_trait]
impl Workload for H2SimWorkload {
    fn name(&self) -> &'static str {
        "h2_simulation"
    }

    async fn run(&mut self, ctx: &SimContext) -> SimulationResult<()> {
        let seed = current_sim_seed();
        // Virtual clock base: captured once; the simulated elapsed `Duration` is
        // added to it so the core sees monotonically advancing `Instant`s. The
        // core only ever compares `Instant`s relatively, so the base is
        // unobservable and never enters the trace.
        //
        // The elapsed duration is accumulated HERE rather than read from
        // `SimContext::time` — see this file's "Why the clock is the harness's
        // and not moonpool's" section. It advances only from the seeded RNG, so
        // it stays a pure function of the seed exactly as `ctx.time()` would.
        let base = Instant::now();
        let mut elapsed = Duration::ZERO;

        let swarm_cfg = if self.swarm {
            let cfg = SwarmConfig::draw(ctx, seed);
            if self.verbose {
                eprintln!("{}", cfg.log_line(seed, true));
            }
            if let Some(sink) = &self.config_sink {
                sink.lock().expect("config sink").push(cfg);
            }
            cfg
        } else {
            let cfg = SwarmConfig::full();
            if self.verbose {
                eprintln!("{}", cfg.log_line(seed, false));
            }
            if let Some(sink) = &self.config_sink {
                sink.lock().expect("config sink").push(cfg);
            }
            cfg
        };

        // Configuration is drawn from the seed, so the ceiling and the window
        // the oracles use are this run's own construction inputs.
        let max_concurrent = 1 + ctx.random().random_range(0..8u32);
        let connection_window = 65_535 + 1024 * ctx.random().random_range(0..16u32);
        let config = H2ConnectionConfig::new(connection_window, max_concurrent, 2);
        let mut harness = H2Harness::new(config, HarnessTimeouts::default(), base);
        let mut model = Model {
            next_stream_id: 1,
            ..Model::default()
        };

        harness.bootstrap(base + elapsed, 65_535);

        for step in 0..self.steps {
            if harness.goaway_seen() {
                // The core has terminated the connection. Every remaining
                // assertion is about what it emitted before that point; driving
                // more client frames into a GOAWAY'd connection tests nothing.
                break;
            }

            // ACK the core's SETTINGS inside its 5 s deadline, whatever the
            // grammar draws — otherwise long scenarios all end in the same
            // `SettingsTimeout` GOAWAY and explore nothing.
            if model.settings_acked_at.is_none() && elapsed >= SETTINGS_ACK_BUDGET {
                harness.feed(&settings_ack());
                model.settings_acked_at = Some(elapsed);
            }

            let action = swarm_cfg.pick(ctx);
            match action {
                Action::OpenStream => {
                    let id = model.allocate_stream();
                    let end_stream = ctx.random().random_range(0..2u8) == 1;
                    if end_stream {
                        model.finish(id);
                    }
                    harness.feed(&headers(id, "sim.invalid", end_stream));
                }
                Action::SendData => {
                    let open: Vec<u32> = model.open_ids().collect();
                    if let Some(id) = pick_one(ctx, &open) {
                        let len = ctx.random().random_range(0..1024usize);
                        model.data_sent += len as u64;
                        let end_stream = ctx.random().random_range(0..4u8) == 0;
                        if !end_stream {
                            *model.data_sent_per_stream.entry(id).or_default() += len as u64;
                        }
                        if end_stream {
                            model.finish(id);
                        }
                        harness.feed(&data(id, len, end_stream));
                    }
                }
                Action::ResetStream => {
                    let open: Vec<u32> = model.open_ids().collect();
                    if let Some(id) = pick_one(ctx, &open) {
                        model.finish(id);
                        harness.feed(&rst_stream(id, E_NO_ERROR));
                    }
                }
                Action::WindowUpdate => {
                    let increment = 1 + ctx.random().random_range(0..65_535u32);
                    model.window_granted += increment as u64;
                    harness.feed(&window_update(0, increment));
                }
                Action::Ping => {
                    harness.feed(&ping(ctx.random().random_range(0..u64::MAX)));
                }
                Action::PeerSettings => {
                    harness.feed(&settings(&[(
                        S_INITIAL_WINDOW_SIZE,
                        1024 * (1 + ctx.random().random_range(0..64u32)),
                    )]));
                }
                Action::SettingsAck => {
                    harness.feed(&settings_ack());
                    model.settings_acked_at.get_or_insert(elapsed);
                }
                Action::AdvanceClock => {
                    // Bounded well below the 30 s stream-idle timeout most of
                    // the time, so scenarios accumulate state rather than being
                    // reaped every other step.
                    let millis = ctx.random().random_range(1..3_000u64);
                    elapsed += Duration::from_millis(millis);
                }
            }

            // Low-probability wire fault: truncate the tail of what is queued so
            // a frame arrives incomplete and stays that way. The core must
            // answer with a GOAWAY or simply keep waiting — never panic.
            if buggify_with_prob!(0.01) {
                let socket = harness.socket_mut();
                let keep = socket.inbound.len().saturating_sub(3);
                socket.inbound.truncate(keep);
            }

            let fragmentation = 1 + ctx.random().random_range(0..64usize);
            let write_budget = 1 + ctx.random().random_range(0..4096usize);
            let result = harness.pump(base + elapsed, fragmentation, write_budget);

            check_invariants(&harness, &model, config, seed, step);

            if matches!(result, MuxResult::CloseSession | MuxResult::Upgrade) {
                break;
            }
        }

        // FINAL: the peer goes away. The core must settle without panicking and
        // must not leave a stream in a state that claims a live backend — no
        // backend was ever attached.
        harness.shutdown_peer();
        harness.pump(base + elapsed, usize::MAX, usize::MAX);
        check_invariants(&harness, &model, config, seed, self.steps);

        assert!(
            harness
                .context
                .streams
                .iter()
                .all(|s| !matches!(s.state, StreamState::Linked(_))),
            "seed={seed:#x}: a stream claims a backend link, but no backend was ever attached",
        );

        if self.verbose {
            eprintln!(
                "seed={seed:#x} DONE steps={} opened={} finished={} peak_open={} frames_out={} fingerprint={:#x}",
                self.steps,
                model.opened.len(),
                model.finished.len(),
                model.peak_open,
                harness.emitted.len(),
                harness.fingerprint(),
            );
        }
        if let Some(sink) = &self.sink {
            sink.lock().expect("fingerprint sink").push((
                harness.fingerprint(),
                model.opened.len(),
                harness.emitted.len(),
            ));
        }

        Ok(())
    }
}

fn pick_one(ctx: &SimContext, ids: &[u32]) -> Option<u32> {
    if ids.is_empty() {
        return None;
    }
    Some(ids[ctx.random().random_range(0..ids.len())])
}

/// Cross-step invariants, all checked against the harness's own inputs.
fn check_invariants(
    harness: &H2Harness,
    model: &Model,
    config: H2ConnectionConfig,
    seed: u64,
    step: usize,
) {
    // Property 3a — the concurrent-stream ceiling. `max_concurrent_streams` is a
    // CONSTRUCTION input of this run; `stream_count` is the core's live count.
    let live = match &harness.connection {
        Connection::H2(h2) => h2.stream_count(),
        Connection::H1(_) => unreachable!("new_h2_server built an H2 connection"),
    };
    // The comparison is against the CONFIGURED ceiling, not against the limit
    // the core advertises at any instant: `ConnectionH2::apply_mcs_backpressure`
    // lowers `local_settings.settings_max_concurrent_streams` below the
    // configured value under refusal pressure, so a `MAX CONCURRENT STREAMS:
    // limit=3, current=7` log line during a sweep is the adaptive limit doing
    // its job against a configured ceiling of 7 or more, not a breach.
    assert!(
        live as u32 <= config.max_concurrent_streams,
        "seed={seed:#x} step={step}: {live} live streams exceeds the configured \
         max_concurrent_streams={}",
        config.max_concurrent_streams,
    );

    // Property 3a, second half — every stream the harness opened past the
    // ceiling must have been REFUSED, not silently dropped. The oracle is the
    // harness's own open count, never anything the core reported.
    let refused = harness
        .emitted_of(T_RST_STREAM)
        .filter(|f| f.rst_error() == Some(E_REFUSED_STREAM))
        .count();
    let over_ceiling = model
        .opened
        .len()
        .saturating_sub(config.max_concurrent_streams as usize + model.finished.len());
    assert!(
        refused >= over_ceiling || harness.goaway_seen(),
        "seed={seed:#x} step={step}: opened={} finished={} ceiling={} implies at least \
         {over_ceiling} REFUSED_STREAM, saw {refused}",
        model.opened.len(),
        model.finished.len(),
        config.max_concurrent_streams,
    );

    // Property 3b — inbound credit is never OVER-returned on a stream.
    //
    // The core hands a peer fresh per-stream credit with a stream-level
    // WINDOW_UPDATE. Returning more than the peer actually spent on that stream
    // is the dangerous direction: it lets the peer send beyond what it was
    // granted, and it is exactly what a mis-attribution across interleaved,
    // byte-fragmented streams looks like. The harness counts what it SENT per
    // stream; the core's own accounting appears on neither side.
    //
    // The randomized sweep asserts only the direction, because a reset or
    // recycled stream legitimately stops earning credit mid-flight;
    // `h2_inbound_credit_is_attributed_to_the_stream_that_spent_it` pins the
    // exact equality on a schedule where nothing is reset.
    let mut credit_returned: BTreeMap<u32, u64> = BTreeMap::new();
    for frame in harness
        .emitted_of(T_WINDOW_UPDATE)
        .filter(|f| f.stream_id != 0)
    {
        *credit_returned.entry(frame.stream_id).or_default() +=
            u64::from(frame.window_increment().unwrap_or(0));
    }
    for (stream_id, returned) in &credit_returned {
        let spent = model
            .data_sent_per_stream
            .get(stream_id)
            .copied()
            .unwrap_or(0);
        assert!(
            *returned <= spent,
            "seed={seed:#x} step={step}: returned {returned} octets of credit on stream \
             {stream_id} but the peer only spent {spent} there",
        );
    }

    // A stream slot is never reused while the harness still considers it open:
    // the recycled-slot bookkeeping is what the ceiling rests on.
    let recycled = harness
        .context
        .streams
        .iter()
        .filter(|s| matches!(s.state, StreamState::Recycle))
        .count();
    assert!(
        recycled + live <= harness.context.streams.len(),
        "seed={seed:#x} step={step}: {recycled} recycled + {live} live exceeds {} slots",
        harness.context.streams.len(),
    );
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

fn swarm_enabled() -> bool {
    !matches!(std::env::var("SOZU_SIM_SWARM"), Ok(v) if v.trim() == "0")
}

fn inclusive_seed(seed: u64) -> bool {
    seed.is_multiple_of(4)
}

fn campaign_seeds(count: usize) -> Vec<u64> {
    (0..u64::try_from(count).expect("campaign seed count fits in u64")).collect()
}

/// moonpool aborts a seed whose simulated time exceeds its run-time budget.
/// `AdvanceClock` moves logical time by up to 3 s per step, so budget
/// proportionally — this is logical time only, which moonpool runs in
/// milliseconds.
fn run_budget(steps: usize) -> Duration {
    Duration::from_secs((steps as u64).saturating_mul(10).saturating_add(3_600))
}

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

fn workload(steps: usize, verbose: bool, swarm: bool) -> H2SimWorkload {
    H2SimWorkload {
        steps,
        verbose,
        sink: None,
        swarm,
        config_sink: None,
    }
}

// --------------------------------------------------------------------------
// Tests.
// --------------------------------------------------------------------------

/// Deterministic seed sweep (FoundationDB nightly seed-sweep / VOPR analog).
/// On a hard invariant violation the panic carries the failing seed + step;
/// moonpool also lists it in `report.seeds_failing`.
#[test]
fn h2_simulation_seed_sweep() {
    let steps = env_usize("SOZU_H2_SIM_STEPS").unwrap_or(400);
    let swarm = swarm_enabled();

    if let Some(seed) = env_u64("SOZU_H2_SIM_SEED") {
        eprintln!("== H2 sim single-seed replay: seed={seed:#x} steps={steps} swarm={swarm} ==");
        let report = SimulationBuilder::new()
            .workload(workload(steps, true, swarm))
            .set_debug_seeds(vec![seed])
            // Without an explicit iteration count `SimulationBuilder` defaults
            // to `UntilCoverageStable` and would keep drawing FRESH seeds after
            // this one — fixing it to 1 is what makes this "replay that ONE
            // seed" (the same fix the UDP and TCP preread replay paths carry).
            .set_iterations(1)
            .run_time_budget(run_budget(steps))
            .run();
        assert_no_failures(&report);
        return;
    }

    let seeds = env_usize("SOZU_H2_SIM_SEEDS").unwrap_or(256);
    let report = SimulationBuilder::new()
        .workload(workload(steps, false, swarm))
        .set_debug_seeds(campaign_seeds(seeds))
        .set_iterations(seeds)
        .run_time_budget(run_budget(steps))
        .run();
    assert_no_failures(&report);
}

/// Fast smoke test pinning one representative seed, swarm off so the pinned
/// trajectory keeps exercising the full grammar.
#[test]
fn h2_simulation_replays_known_seed() {
    let steps = 600;
    let report = SimulationBuilder::new()
        .workload(workload(steps, false, false))
        .set_debug_seeds(vec![0x5E_ED_C0_DE])
        .set_iterations(1)
        .run_time_budget(run_budget(steps))
        .run();
    assert_no_failures(&report);
}

/// **Property 1 — determinism.**
///
/// The same seed must yield a byte-identical observable trace, and different
/// seeds must not collapse onto one trajectory. A divergence on the first half
/// means the core grew a hidden nondeterministic dependency (wall clock, a
/// random source, hash-map iteration order leaking into an output); a collapse
/// on the second half means the harness is not actually exploring.
#[test]
fn h2_simulation_is_deterministic() {
    fn fingerprint(seed: u64) -> (u64, usize, usize) {
        let steps = 300;
        let sink: FingerprintSink = Arc::new(Mutex::new(Vec::new()));
        let report = SimulationBuilder::new()
            .workload(H2SimWorkload {
                steps,
                verbose: false,
                sink: Some(sink.clone()),
                // Swarm stays ON so the guard also covers the config draw: a
                // nondeterministic draw would fork the trace.
                swarm: true,
                config_sink: None,
            })
            .set_debug_seeds(vec![seed])
            .set_iterations(1)
            .run_time_budget(run_budget(steps))
            .run();
        assert_no_failures(&report);
        let recorded = sink.lock().expect("fingerprint sink");
        *recorded.first().expect("workload recorded a fingerprint")
    }

    let a = fingerprint(0x00AB_CDEF);
    let b = fingerprint(0x00AB_CDEF);
    assert_eq!(
        a, b,
        "same seed must yield an identical observable trace (determinism)"
    );

    // Different seeds must explore different interleavings. Four seeds, at
    // least two distinct fingerprints: a harness whose trace is a constant
    // would pass the equality above and prove nothing.
    let explored: std::collections::BTreeSet<u64> = [0x11u64, 0x12, 0x13, 0x14]
        .into_iter()
        .map(|seed| fingerprint(seed).0)
        .collect();
    assert!(
        explored.len() >= 2,
        "distinct seeds collapsed onto {} trajectory/ies — the sweep explores nothing",
        explored.len(),
    );
}

/// **Property 2 — the timer wheel's earliness window is not an expiry.**
///
/// `crate::timer`'s `duration_to_tick` rounds a delay to the NEAREST tick, so an
/// entry armed for `D` is delivered from `D - tick/2` onwards and the total
/// earliness is `(delay_ms + tick_ms/2) mod tick_ms` — `[0, 99]` ms at the
/// default 100 ms tick. That class is `timer.rs`'s documented behaviour, pinned
/// there by `test_maximum_earliness_is_a_full_half_tick`; this test takes it as
/// ground truth rather than re-deriving it, and asserts the CORE-side
/// consequence a unit test cannot reach: driven at EVERY millisecond of that
/// window the core must leave the stream alone, and only strictly past the
/// deadline may it retire it — exactly once.
///
/// This is the claim C1's re-validation exists to protect, expressed on frames
/// instead of on `Mux::sync_timeout`.
///
/// The PING that rides through the window is not decoration. It makes each
/// probe a real pass — the core reads a frame, queues an ACK and writes it —
/// so "no RST_STREAM" is a statement about a connection that was genuinely
/// exercised at that instant, not about one that was never woken.
///
/// # A measured gap this test deliberately does not assert
///
/// The CONNECTION-level deadline (`Connection::poll_timeout`) does not hold the
/// same line, and no assertion here pretends it does (sozu-proxy/sozu#1489).
/// `ConnectionH2::poll_read_target` documents the policy — "H2 control frames
/// (PING, WINDOW_UPDATE, SETTINGS) must NOT reset it, otherwise a peer sending
/// periodic PINGs prevents timeout detection on stuck sessions" — but
/// `ConnectionH2::write_streams` opens with an unconditional
/// `ConnectionH2::arm_timeout`, so any pass that has something to write reaches
/// it. Measured on `f0a45568` with a 600 s connection timeout, driving this
/// harness at a fixed instant per pass with a 600 s connection timeout, opening
/// one stream at t=10 ms, and reading `Connection::poll_timeout` after each
/// pass: HEADERS at t=10 ms arms it to t=10 ms (correct — application
/// activity), a PING at t=10 s moves it to t=10 s, an empty pass at t=20 s
/// leaves it, a client WINDOW_UPDATE at t=30 s leaves it, a client SETTINGS at
/// t=40 s moves it to t=40 s, and a second PING at t=500 s moves it to t=500 s.
/// The three that move are exactly the three that make the core queue something
/// to send. That last probe is the consequence: with no application activity
/// since t=10 ms, a peer that PINGs anywhere inside the 600 s window keeps the
/// session alive forever, which is the scenario the policy comment names. The
/// PER-STREAM deadline asserted below is immune — it is what still retires the
/// stream — so this is a gap in the connection-level guard, not in both.
#[test]
fn h2_stream_idle_deadline_survives_the_wheel_earliness_window() {
    const TICK_MS: u64 = 100;
    let idle = Duration::from_secs(30);
    let timeouts = HarnessTimeouts {
        connection: Duration::from_secs(600),
        stream_idle: idle,
    };

    // The earliness class for this deadline, computed from `duration_to_tick`'s
    // documented formula and NOT from any Sōzu call.
    let max_earliness_ms = (idle.as_millis() as u64 + TICK_MS / 2) % TICK_MS;
    assert_eq!(
        max_earliness_ms, 50,
        "a 30 s deadline sits at (30000 + 50) mod 100 = 50 ms of earliness"
    );

    // Probe the whole documented window, not just this deadline's own class: an
    // event loop reaches any point in [0, tick-1] through another session's
    // entry plus loop latency. `0` is included on purpose — it is the deadline
    // itself, which `H2StreamTable::collect_timed_out` compares with a STRICT
    // `now - last_activity > deadline`, so the instant `D` is still inside the
    // safe window and retirement happens strictly after it.
    let probes: Vec<u64> = (0..TICK_MS).collect();

    for early_ms in probes {
        let base = Instant::now();
        let mut harness = H2Harness::new(H2ConnectionConfig::default(), timeouts, base);

        harness.bootstrap(base, 65_535);
        harness.feed(&settings_ack());
        // One stream, opened and then left completely silent.
        harness.feed(&headers(1, "sim.invalid", false));
        let opened_at = base + Duration::from_millis(10);
        harness.pump(opened_at, usize::MAX, usize::MAX);

        let resets_after_open = harness
            .emitted_of(T_RST_STREAM)
            .filter(|f| f.stream_id == 1)
            .count();
        assert_eq!(
            resets_after_open, 0,
            "early_ms={early_ms}: a freshly opened stream must not be reset"
        );

        // Deliver a timer callback at `deadline - early_ms`, the instant an
        // early wheel delivery would hand the core. The PING rides along so the
        // pass also carries a real readable/writable cycle rather than being a
        // bare timeout with nothing else happening.
        let early_instant = opened_at + idle - Duration::from_millis(early_ms);
        harness.feed(&ping(0xA5A5_A5A5_A5A5_A5A5));
        harness.timeout_pass(early_instant);

        assert_eq!(
            harness
                .emitted_of(T_RST_STREAM)
                .filter(|f| f.stream_id == 1)
                .count(),
            0,
            "early_ms={early_ms}: the core retired stream 1 at deadline-{early_ms}ms — \
             a wheel delivery inside the [0, {}] ms earliness window is not an expiry",
            TICK_MS - 1,
        );
        assert_eq!(
            harness.h2().stream_count(),
            1,
            "early_ms={early_ms}: the stream left the table inside the earliness window",
        );
        assert!(
            !harness.goaway_seen(),
            "early_ms={early_ms}: the core GOAWAY'd inside the earliness window",
        );

        // Strictly past the deadline the stream is retired — exactly once.
        harness.timeout_pass(opened_at + idle + Duration::from_millis(1));
        assert_eq!(
            harness
                .emitted_of(T_RST_STREAM)
                .filter(|f| f.stream_id == 1)
                .count(),
            1,
            "early_ms={early_ms}: the stream-idle deadline did not retire stream 1 \
             exactly once past the deadline",
        );
    }
}

/// **Property 3a — the concurrent-stream ceiling holds under adversarial
/// interleaving.**
///
/// `max_concurrent_streams` is a construction input. The harness opens well past
/// it while frame headers straddle `socket_read` calls and writes complete only
/// partially, then checks the live count against that input — never against
/// anything the core reported — and requires the excess to have been REFUSED
/// rather than silently dropped.
#[test]
fn h2_concurrent_stream_ceiling_holds_under_adversarial_interleaving() {
    const CEILING: u32 = 3;
    let config = H2ConnectionConfig::new(65_535, CEILING, 2);

    // Two adversarial schedules over the same script: whole-frame delivery, and
    // one byte per read with one byte per write. The second is the interesting
    // one — every 9-octet frame header arrives across nine `socket_read` calls
    // and every answer leaves one octet at a time.
    for (schedule, fragmentation, write_budget) in [
        ("whole-frames", usize::MAX, usize::MAX),
        ("one-octet-at-a-time", 1usize, 1usize),
    ] {
        let base = Instant::now();
        let mut harness = H2Harness::new(config, HarnessTimeouts::default(), base);
        harness.bootstrap(base, 65_535);
        harness.feed(&settings_ack());

        let opens = 12u32;
        for index in 0..opens {
            let id = 1 + index * 2;
            harness.feed(&headers(id, "sim.invalid", false));
            harness.pump(
                base + Duration::from_millis(10 * (index as u64 + 1)),
                fragmentation,
                write_budget,
            );
            let live = match &harness.connection {
                Connection::H2(h2) => h2.stream_count(),
                Connection::H1(_) => unreachable!(),
            };
            assert!(
                live as u32 <= CEILING,
                "schedule={schedule}: {live} live streams after opening {} exceeds the \
                 configured ceiling {CEILING}",
                index + 1,
            );
        }

        let refused = harness
            .emitted_of(T_RST_STREAM)
            .filter(|f| f.rst_error() == Some(E_REFUSED_STREAM))
            .count();
        assert_eq!(
            refused as u32,
            opens - CEILING,
            "schedule={schedule}: opening {opens} streams against a ceiling of {CEILING} \
             must refuse exactly {} of them, saw {refused}",
            opens - CEILING,
        );
    }
}

/// **Property 3c — buffer-pool exhaustion mid-request refuses the stream and
/// keeps the connection.**
///
/// The concurrent-stream ceiling above is an admission decision the core makes
/// from its own bookkeeping. This one is the other refusal path, and it is the
/// one that constrains buffer ownership: a stream needs a request/response
/// buffer pair the core does not own and cannot conjure, the supply is
/// exhausted *while the connection is healthy and mid-request*, and the answer
/// must still be `RST_STREAM(REFUSED_STREAM)` on that one stream — never
/// GOAWAY, never a dropped frame, never a stalled connection. Exhaustion is
/// transient by nature: the streams already running will hand their buffers
/// back, so tearing the connection down over it converts a momentary shortage
/// into every in-flight request on that connection being lost.
///
/// The pool is sized so the shortage is arithmetic rather than incidental:
/// three buffers total, one consumed by the connection's own `zero` buffer at
/// construction and two by the first stream, leaving the second stream's pair
/// unsatisfiable. `max_concurrent_streams` is set far above the two streams
/// opened, and the assertions below require the *admitted* stream to still be
/// live, so a run in which the ceiling refused instead would fail rather than
/// pass for the wrong reason.
///
/// Nothing here reads a log line or any other core-internal literal: the
/// oracle is the wire, decoded by this file's own decoder.
#[test]
fn h2_pool_exhaustion_mid_request_refuses_the_stream_not_the_connection() {
    // Far above the two streams opened below: the pool, not the ceiling, must
    // be what refuses.
    let config = H2ConnectionConfig::new(65_535, 64, 2);
    let base = Instant::now();
    // 1 (`zero`) + 2 (stream 1) = 3. Stream 3 then finds nothing.
    let mut harness = H2Harness::with_pool_maximum(config, HarnessTimeouts::default(), base, 3);

    harness.bootstrap(base, 65_535);
    harness.feed(&settings_ack());

    harness.feed(&headers(1, "sim.invalid", false));
    harness.pump(base + Duration::from_millis(10), usize::MAX, usize::MAX);
    assert_eq!(
        harness.h2().stream_count(),
        1,
        "the first stream must be admitted — the pool still had its pair",
    );
    assert_eq!(
        harness.emitted_of(T_RST_STREAM).count(),
        0,
        "nothing was refused before the pool ran out",
    );

    // The pool is now empty. This is the mid-request exhaustion.
    harness.feed(&headers(3, "sim.invalid", false));
    harness.pump(base + Duration::from_millis(20), usize::MAX, usize::MAX);

    let refusals: Vec<&WireFrame> = harness
        .emitted_of(T_RST_STREAM)
        .filter(|f| f.rst_error() == Some(E_REFUSED_STREAM))
        .collect();
    assert_eq!(
        refusals.len(),
        1,
        "exhaustion must refuse exactly the one stream it could not serve, saw {} RST_STREAM \
         frames in total",
        harness.emitted_of(T_RST_STREAM).count(),
    );
    assert_eq!(
        refusals[0].stream_id, 3,
        "the refusal must name the stream that could not be served",
    );
    assert!(
        !harness.goaway_seen(),
        "buffer-pool exhaustion is transient and MUST NOT be escalated to a connection error",
    );
    assert_eq!(
        harness.h2().stream_count(),
        1,
        "the stream that was already admitted must survive the refusal of another",
    );

    // And the connection is still usable: the admitted stream can still be
    // driven to completion, which is the whole point of refusing rather than
    // GOAWAYing. A core that had torn the connection down would emit nothing
    // more and leave the stream parked.
    harness.feed(&data(1, 64, true));
    harness.pump(base + Duration::from_millis(30), usize::MAX, usize::MAX);
    assert!(
        !harness.goaway_seen(),
        "the connection must still be alive after the refused stream",
    );
    assert_eq!(
        harness
            .emitted_of(T_RST_STREAM)
            .filter(|f| f.stream_id == 1)
            .count(),
        0,
        "the surviving stream must not have been reset by another stream's refusal",
    );
}

/// **Property 3b — inbound flow-control credit is attributed to the stream
/// that spent it, under interleaved and byte-fragmented delivery.**
///
/// Four streams are fed DATA round-robin while every `socket_read` returns at
/// most 17 octets and every `socket_write` at most 257 — so a DATA frame's
/// 9-octet header routinely arrives across two reads and the core's answers
/// leave in pieces. Under that schedule the per-stream WINDOW_UPDATE credit the
/// core returns must equal, exactly, the octets the harness spent on that same
/// stream. One octet credited to the wrong stream would let a peer overrun one
/// window while starving another, and it is invisible to an end-to-end test,
/// which cannot choose where a frame header splits.
///
/// Both sides of the comparison are the harness's own: the octets it sent, and
/// the increments it decoded off the wire with its own decoder.
///
/// What this does NOT claim is that the core refuses DATA past the
/// CONNECTION-level window it advertised — see this file's property 3 note; it
/// does not, and no assertion here is shaped to pretend otherwise.
#[test]
fn h2_inbound_credit_is_attributed_to_the_stream_that_spent_it() {
    let config = H2ConnectionConfig::new(65_535, 8, 2);
    let base = Instant::now();
    let mut harness = H2Harness::new(config, HarnessTimeouts::default(), base);

    harness.bootstrap(base, 65_535);
    harness.feed(&settings_ack());

    let ids = [1u32, 3, 5, 7];
    for id in ids {
        harness.feed(&headers(id, "sim.invalid", false));
    }
    harness.pump(base + Duration::from_millis(1), usize::MAX, usize::MAX);

    // Deliberately uneven per-stream volumes: equal ones would make a swap
    // between two streams undetectable.
    //
    // The totals stay well under the 16 KiB per-stream buffer this harness's
    // pool hands out. That bound is load-bearing, not cosmetic: no backend is
    // attached, so nothing drains a stream's request buffer, and a stream whose
    // buffer fills makes the core stop reading — correct back-pressure, but it
    // would leave octets sitting in the harness's own inbound queue that the
    // oracle below counts as spent and the core never saw. The
    // `inbound_len() == 0` assertion after the loop is what holds this to a
    // fact instead of a hope.
    let chunks = [128usize, 512, 1024, 333];
    let mut spent: BTreeMap<u32, u64> = BTreeMap::new();
    for round in 0..10u64 {
        for (id, chunk) in ids.iter().zip(chunks.iter()) {
            harness.feed(&data(*id, *chunk, false));
            *spent.entry(*id).or_default() += *chunk as u64;
        }
        // 17 and 257 are coprime with the 9-octet frame header and with every
        // chunk length above, so the split point walks across the frames
        // instead of landing on the same boundary every round.
        harness.pump(base + Duration::from_millis(10 * (round + 1)), 17, 257);
    }

    assert!(
        !harness.goaway_seen(),
        "the core GOAWAY'd during a conformant multi-stream DATA schedule",
    );
    assert_eq!(
        harness.inbound_len(),
        0,
        "the core left octets unread, so `spent` below counts bytes it never saw — \
         re-tune the volumes under the per-stream buffer rather than relaxing the \
         comparison",
    );

    let mut returned: BTreeMap<u32, u64> = BTreeMap::new();
    for frame in harness
        .emitted_of(T_WINDOW_UPDATE)
        .filter(|f| f.stream_id != 0)
    {
        *returned.entry(frame.stream_id).or_default() += u64::from(
            frame
                .window_increment()
                .expect("WINDOW_UPDATE carries an increment"),
        );
    }

    assert_eq!(
        returned, spent,
        "per-stream inbound credit returned must equal the octets spent on that stream",
    );
    // Guard against a vacuous pass: the maps must be non-trivial and distinct
    // per stream, or an all-zero equality would satisfy the assertion above.
    assert_eq!(
        returned.len(),
        ids.len(),
        "every stream must have earned credit"
    );
    assert!(
        returned.values().copied().collect::<BTreeSet<u64>>().len() == ids.len(),
        "the four streams must have earned four DISTINCT credit totals, else a swap \
         between two of them would be undetectable: {returned:?}",
    );
}

/// Swarm-config stability: the configuration drawn for a seed is a pure function
/// of that seed, so a failing seed's `swarm-config` line replays byte-identically.
#[test]
fn h2_swarm_config_is_stable_across_draws() {
    fn draw(seed: u64) -> SwarmConfig {
        let steps = 8;
        let sink: ConfigSink = Arc::new(Mutex::new(Vec::new()));
        let report = SimulationBuilder::new()
            .workload(H2SimWorkload {
                steps,
                verbose: false,
                sink: None,
                swarm: true,
                config_sink: Some(sink.clone()),
            })
            .set_debug_seeds(vec![seed])
            .set_iterations(1)
            .run_time_budget(run_budget(steps))
            .run();
        assert_no_failures(&report);
        let recorded = sink.lock().expect("config sink");
        *recorded.first().expect("workload recorded a swarm config")
    }

    for seed in [0x0Au64, 0x0B, 0x0C, 0x5EED_5EED, 0xDEAD_BEEF] {
        let a = draw(seed);
        let b = draw(seed);
        assert_eq!(
            a, b,
            "seed={seed:#x}: swarm config must be identical across two draws"
        );
        assert!(
            a.is_enabled(Action::OpenStream),
            "seed={seed:#x}: the MANDATORY OpenStream feature must always be enabled"
        );
        assert_eq!(
            a.enabled.iter().all(|e| *e),
            inclusive_seed(seed),
            "seed={seed:#x}: only reserved seeds may use the full grammar"
        );
        assert_eq!(a.log_line(seed, true), b.log_line(seed, true));
    }
}

#[test]
fn h2_campaign_reserves_one_inclusive_seed_per_four() {
    for count in [1usize, 3, 4, 5, 256] {
        let seeds = campaign_seeds(count);
        let inclusive = seeds.iter().filter(|seed| inclusive_seed(**seed)).count();
        assert_eq!(inclusive, count.div_ceil(4), "count={count}");
        assert_eq!(seeds.len() - inclusive, count - count.div_ceil(4));
    }
}

/// The hand-rolled encoder and decoder are each other's inverse, and neither
/// touches `sozu_lib::protocol::mux::parser`. A round-trip guard here is what
/// keeps the oracle trustworthy: every assertion above reads frames through
/// [`decode_frames`].
#[test]
fn h2_wire_codec_round_trips() {
    let script = [
        settings(&[
            (S_INITIAL_WINDOW_SIZE, 65_535),
            (S_MAX_CONCURRENT_STREAMS, 7),
        ]),
        settings_ack(),
        headers(1, "sim.invalid", false),
        data(1, 300, true),
        rst_stream(3, E_REFUSED_STREAM),
        window_update(0, 1024),
        ping(0xDEAD_BEEF_CAFE_BABE),
    ];
    let wire: Vec<u8> = script.concat();
    let (frames, consumed) = decode_frames(&wire);
    assert_eq!(consumed, wire.len(), "every byte must belong to a frame");
    assert_eq!(frames.len(), script.len());
    assert_eq!(frames[0].ty, T_SETTINGS);
    assert_eq!(frames[1].flags, F_ACK);
    assert_eq!(frames[2].ty, T_HEADERS);
    assert_eq!(frames[3].payload.len(), 300);
    assert_eq!(frames[3].flags & F_END_STREAM, F_END_STREAM);
    assert_eq!(frames[4].rst_error(), Some(E_REFUSED_STREAM));
    assert_eq!(frames[5].window_increment(), Some(1024));
    assert_eq!(frames[6].ty, T_PING);

    // A truncated tail is left whole for the next call, never half-decoded.
    let truncated = &wire[..wire.len() - 3];
    let (partial, partial_consumed) = decode_frames(truncated);
    assert_eq!(partial.len(), script.len() - 1);
    assert!(partial_consumed < truncated.len());
}

#[test]
fn env_parse_accepts_hex_and_decimal() {
    assert_eq!(parse_u64("42"), Some(42));
    assert_eq!(parse_u64(" 256 "), Some(256));
    assert_eq!(parse_u64("0xdeadbeef"), Some(0xdead_beef));
    assert_eq!(parse_u64("0XFF"), Some(0xFF));
    assert_eq!(parse_u64("notanumber"), None);
}
