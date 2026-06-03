//! Deterministic simulation test (FoundationDB/VOPR-style) for the sans-io UDP
//! core (`sozu_lib::protocol::udp`).
//!
//! In the spirit of TigerBeetle's VOPR and FoundationDB's simulator: a single
//! seeded `StdRng` drives a randomized, adversarial workload against
//! [`UdpManager`] across many seeds. Same seed → bit-for-bit identical run, so
//! every failure reproduces exactly from its seed + step.
//!
//! ## Why deterministic simulation fits here
//!
//! The UDP core is **pure sans-io**: time is an injected `now: Instant`, the
//! hash seed is injected at construction, there is no socket, no
//! `Instant::now()` in the hot path, and no `rand`. That is precisely the
//! property that makes deterministic replay possible. A sibling change packed
//! the core with `debug_assert` invariants plus a `check_invariants()`
//! post-condition that runs at the end of every public mutating method (in
//! debug / test builds). This harness's job is to drive the core hard enough,
//! across enough seeds, that those embedded assertions — and the higher-level
//! model invariants this file adds — actually fire on any regression.
//!
//! ## Virtual clock
//!
//! A base `Instant` is captured **once**, outside the stepping loop, and
//! advanced only by RNG-drawn deltas. No wall-clock read happens inside the
//! loop, so a run is a pure function of its seed.
//!
//! ## Action grammar (weighted)
//!
//! Each step draws one weighted-random action — see [`Action`] and `pick_action`
//! for the exact weights. The mix stresses admission, the await/established
//! transition, NAT return, reconfig storms, cap shrink/grow, drain, idle
//! reaping, and on-demand teardown.
//!
//! ## Buggify
//!
//! [`buggify`] is the FoundationDB-`buggify` analog: with low per-call
//! probability it injects an *extra* adversarial event (stale `BackendResolved`,
//! a reconfig burst, a `max_flows` shrink to a tiny value, a giant clock jump).
//! It only ever runs under simulation.
//!
//! ## Harness invariants (in addition to the core's embedded asserts)
//!
//! Checked every step in [`Model::check`]:
//! - `flow_count() <= high-water mark of every cap ever set`.
//! - after fully draining, `poll_output()` is `None`.
//! - `poll_timeout()` is `None` (no live flows) or strictly `> now` after a
//!   `handle_timeout` that fired.
//! - model balance: `created_seen - evicted_seen == flow_count()` (active-flow
//!   accounting; no underflow, no leak).
//! - no panic for any input (empty / oversized / stale / unknown).
//! - FINAL: after a giant clock jump + `close_all`, the manager drains to zero —
//!   `flow_count() == 0`, `poll_timeout() == None`, `created == evicted`.
//!
//! ## Replay / sweep ergonomics (FoundationDB / VOPR)
//!
//! - [`udp_simulation_seed_sweep`] runs seeds `0..256` (a few thousand steps
//!   each) deterministically. On failure the panic prints the failing seed +
//!   step so it reproduces.
//! - Env overrides:
//!   - `SOZU_UDP_SIM_SEED=<u64|0xhex>` — run that ONE seed with verbose tracing
//!     (replay a CI failure).
//!   - `SOZU_UDP_SIM_SEEDS=<n>` — widen the sweep to `0..n` seeds.
//!   - `SOZU_UDP_SIM_STEPS=<n>` — deepen each run to `n` steps.
//! - [`udp_simulation_replays_known_seed`] pins one representative seed as a
//!   fast end-to-end smoke test.

use std::{
    net::{IpAddr, Ipv4Addr, SocketAddr},
    time::{Duration, Instant},
};

use rand::{RngExt, SeedableRng, rngs::StdRng};

use sozu_lib::protocol::udp::{
    CloseReason, ClusterConfig, ConfigEvent, FlowId, ManagerInput, MetricEvent, Output, UdpManager,
};

// --------------------------------------------------------------------------
// Source pool: a small, bounded set of client tuples so flow keys collide and
// get reused realistically (8 IPs × 4 ports = 32 distinct 4-tuples).
// --------------------------------------------------------------------------

const POOL_IPS: u8 = 8;
const POOL_PORTS: u16 = 4;
const POOL_PORT_BASE: u16 = 9000;

/// Pick one of the 32 pooled client source addresses.
fn pooled_source(rng: &mut StdRng) -> SocketAddr {
    let ip = rng.random_range(0..POOL_IPS);
    let port = POOL_PORT_BASE + rng.random_range(0..POOL_PORTS);
    SocketAddr::new(IpAddr::V4(Ipv4Addr::new(10, 0, 0, ip)), port)
}

/// A small set of distinct backend addresses for resolution replies.
fn pooled_backend(rng: &mut StdRng) -> (String, SocketAddr) {
    let n = rng.random_range(0..4u8);
    (
        format!("b{n}"),
        SocketAddr::new(IpAddr::V4(Ipv4Addr::new(127, 0, 0, 1)), 5300 + n as u16),
    )
}

// --------------------------------------------------------------------------
// Shadow model. Tracks just enough to assert active-flow accounting and the
// cap high-water bound without duplicating the core's internals.
// --------------------------------------------------------------------------

struct Model {
    /// `FlowCreated` metrics observed (admissions).
    created_seen: u64,
    /// `FlowEvicted` metrics observed (any close: idle / cap / abort / drain).
    evicted_seen: u64,
    /// Every flow id for which we have seen a `SelectBackend`. The shell is
    /// expected to reply `BackendResolved` for these; we honour a subset.
    awaiting: Vec<FlowId>,
    /// Flows we have resolved (so backend datagrams have a plausible target).
    /// These ids may already be closed — the core must tolerate stale ids.
    established: Vec<FlowId>,
    /// Highest `max_flows` cap ever in force (construction + every SetMaxFlows).
    cap_high_water: usize,
    /// `FlowShed` metrics observed (diagnostic only).
    shed_seen: u64,
    /// Peak concurrent live flows observed (coverage diagnostic).
    peak_flows: usize,
}

impl Model {
    fn new(initial_cap: usize) -> Self {
        Model {
            created_seen: 0,
            evicted_seen: 0,
            awaiting: Vec::new(),
            established: Vec::new(),
            cap_high_water: initial_cap,
            shed_seen: 0,
            peak_flows: 0,
        }
    }

    /// Fully drain the manager's output queue, folding each `Output` into the
    /// shadow model. Returns the list of `SelectBackend` flow ids surfaced this
    /// drain so the caller can resolve a subset.
    fn drain(&mut self, mgr: &mut UdpManager) -> Vec<(FlowId, u64)> {
        let mut selects = Vec::new();
        while let Some(out) = mgr.poll_output() {
            match out {
                Output::Metric(MetricEvent::FlowCreated) => self.created_seen += 1,
                Output::Metric(MetricEvent::FlowEvicted) => self.evicted_seen += 1,
                Output::Metric(MetricEvent::FlowShed) => self.shed_seen += 1,
                Output::SelectBackend { flow, key, .. } => selects.push((flow, key)),
                // The remaining outputs (SendToBackend / SendToClient /
                // OpenUpstream / ArmTimer / Drop / other Metric) are byte/IO
                // side effects the pure core hands to the shell. The simulator
                // has no socket, so it simply consumes them — the point is that
                // draining them is panic-free and leaves the queue empty.
                _ => {}
            }
        }
        selects
    }

    /// Harness-level invariants, checked after every fully-drained step. The
    /// core's own `debug_assert` invariants fire for free during each
    /// `handle_*` call; these add the cross-step model checks the core cannot
    /// see.
    fn check(&mut self, mgr: &mut UdpManager, seed: u64, step: usize, ctx: &str) {
        self.peak_flows = self.peak_flows.max(mgr.flow_count());

        // (a) cap high-water bound: live flows never exceed the largest cap
        // ever in force (a shrink below the live count is legal and must not
        // evict, but must also never let the population grow past the bound).
        assert!(
            mgr.flow_count() <= self.cap_high_water,
            "seed={seed:#x} step={step} [{ctx}]: flow_count {} exceeds cap high-water {}",
            mgr.flow_count(),
            self.cap_high_water,
        );

        // (b) the queue is fully drained before this check runs.
        assert!(
            mgr.poll_output().is_none(),
            "seed={seed:#x} step={step} [{ctx}]: output queue not drained to None",
        );

        // (c) active-flow accounting balances with no underflow / leak.
        assert!(
            self.created_seen >= self.evicted_seen,
            "seed={seed:#x} step={step} [{ctx}]: evicted {} > created {} (gauge underflow)",
            self.evicted_seen,
            self.created_seen,
        );
        let active = self.created_seen - self.evicted_seen;
        assert_eq!(
            active as usize,
            mgr.flow_count(),
            "seed={seed:#x} step={step} [{ctx}]: model balance created-evicted={active} \
             != flow_count {}",
            mgr.flow_count(),
        );

        // (d) timer coherence at the harness level: no live flows => no armed
        // timer; live flows => some armed deadline. (The strict-advance "next
        // deadline > now after a firing" is asserted inside handle_timeout
        // itself in debug builds.)
        assert_eq!(
            mgr.poll_timeout().is_some(),
            mgr.flow_count() > 0,
            "seed={seed:#x} step={step} [{ctx}]: timer-coherence armed={:?} but flow_count={}",
            mgr.poll_timeout(),
            mgr.flow_count(),
        );
    }
}

// --------------------------------------------------------------------------
// Workload grammar.
// --------------------------------------------------------------------------

/// One adversarial action. Weights are assigned in `pick_action`.
#[derive(Clone, Copy, Debug)]
enum Action {
    /// A client datagram from the pooled sources (random payload, incl. empty
    /// and oversized > max_rx).
    ClientDatagram,
    /// A backend reply for a random flow id (established, awaiting, or stale).
    BackendDatagram,
    /// Resolve a backend for a random awaiting flow id (valid path), or for a
    /// random/stale id (UnknownFlow path).
    BackendResolved,
    /// Reconfigure the cluster (flip affinity / responses / requests / PPv2 /
    /// timeouts).
    ReconfigCluster,
    /// Shrink or grow `max_flows`.
    SetMaxFlows,
    /// Change `max_rx_datagram_size`.
    SetMaxRx,
    /// Begin draining.
    Drain,
    /// Advance the virtual clock and fire the reaper.
    AdvanceClock,
    /// Abort a random flow id (on-demand teardown).
    AbortFlow,
    /// Close every flow.
    CloseAll,
}

/// Draw a weighted-random action. Weights are tuned so `ClientDatagram` and
/// `AdvanceClock` dominate (the steady-state datapath), with the disruptive
/// actions (drain / close_all / cap shrink) rarer so flows actually accumulate
/// and get exercised before being torn down.
///
/// | weight | action            |
/// |--------|-------------------|
/// |   34   | ClientDatagram    |
/// |   16   | BackendResolved   |
/// |   14   | BackendDatagram   |
/// |   14   | AdvanceClock      |
/// |    6   | ReconfigCluster   |
/// |    5   | SetMaxFlows       |
/// |    4   | AbortFlow         |
/// |    3   | SetMaxRx          |
/// |    2   | Drain             |
/// |    2   | CloseAll          |
fn pick_action(rng: &mut StdRng) -> Action {
    // Cumulative weights summing to 100.
    let roll = rng.random_range(0..100u32);
    match roll {
        0..34 => Action::ClientDatagram,
        34..50 => Action::BackendResolved,
        50..64 => Action::BackendDatagram,
        64..78 => Action::AdvanceClock,
        78..84 => Action::ReconfigCluster,
        84..89 => Action::SetMaxFlows,
        89..93 => Action::AbortFlow,
        93..96 => Action::SetMaxRx,
        96..98 => Action::Drain,
        _ => Action::CloseAll,
    }
}

/// FoundationDB-`buggify` analog: with low probability, return `true` so the
/// caller injects an *extra* adversarial event this step. Only ever active
/// under simulation (this whole file is a `#[cfg(test)]` integration test).
fn buggify(rng: &mut StdRng, p: f64) -> bool {
    rng.random_bool(p)
}

/// A randomized cluster config (always non-empty cluster name so datagrams have
/// somewhere to route; an empty name is exercised separately via reconfig
/// occasionally — see below).
fn random_cluster(rng: &mut StdRng, allow_empty: bool) -> ClusterConfig {
    let cluster = if allow_empty && rng.random_bool(0.1) {
        // Occasionally drop routing entirely → NoBackend drop path for new
        // flows. Existing flows keep their captured config.
        String::new()
    } else {
        format!("cluster-{}", rng.random_range(0..3u8))
    };
    ClusterConfig {
        cluster,
        affinity_with_port: rng.random_bool(0.5),
        // Caps mostly unlimited (0) so flows persist and exercise the
        // Established / NAT-return / idle-reaper paths; sometimes a small cap so
        // the responses/requests teardown fires too.
        responses: if rng.random_bool(0.6) {
            0
        } else {
            rng.random_range(1..4u32)
        },
        requests: if rng.random_bool(0.6) {
            0
        } else {
            rng.random_range(1..6u32)
        },
        // Longer than the typical small clock jump so flows survive many steps.
        front_timeout: Duration::from_millis(rng.random_range(500..5000)),
        back_timeout: Duration::from_millis(rng.random_range(500..5000)),
        send_proxy_protocol: rng.random_bool(0.3),
        proxy_protocol_every_datagram: rng.random_bool(0.5),
    }
}

/// A random payload sized relative to the live `max_rx`: empty (Invalid drop),
/// tiny, typical, or oversized (`> max_rx`, exercising the Truncated drop path).
/// Capped so the test stays fast.
fn random_payload(rng: &mut StdRng, max_rx: usize) -> Vec<u8> {
    let over = max_rx.saturating_add(1).max(2);
    let len = match rng.random_range(0..16u32) {
        0 => 0,                                                // empty → Invalid drop (rare)
        1..3 => rng.random_range(1..16),                       // tiny
        3..13 => rng.random_range(1..=max_rx.max(1)).min(512), // typical, within cap
        _ => rng.random_range(over..=over + 2048),             // oversized → Truncated drop
    };
    vec![rng.random::<u8>(); len]
}

// --------------------------------------------------------------------------
// The simulation driver.
// --------------------------------------------------------------------------

/// Run one deterministic simulation for `seed` over `steps` steps. Panics with
/// the seed + step on any invariant violation; the core's embedded
/// `debug_assert`s panic from inside the `handle_*` calls (also reproducible
/// from the seed). `verbose` prints a compact trace for single-seed replay.
fn run_simulation(seed: u64, steps: usize, verbose: bool) {
    let mut rng = StdRng::seed_from_u64(seed);

    // Virtual clock: captured ONCE, outside the loop. Everything after this is
    // a pure function of the seed.
    let base = Instant::now();
    let mut now = base;

    // Initial manager config.
    let initial_cap = rng.random_range(1..32usize);
    let initial_max_rx = 1500usize;
    let cluster = random_cluster(&mut rng, false);
    let hash_seed: u64 = rng.random();
    let mut mgr = UdpManager::new(cluster, initial_cap, initial_max_rx, hash_seed);
    let mut model = Model::new(initial_cap);

    // The manager-side max_rx as the simulator currently believes it (so the
    // payload generator can occasionally exceed it). The core owns the real
    // value; this is only a hint for the workload.
    let mut max_rx = initial_max_rx;

    // Drain any construction-time outputs (there are none, but keep the
    // contract uniform) and run the entry invariant.
    let _ = model.drain(&mut mgr);
    model.check(&mut mgr, seed, 0, "init");

    for step in 1..=steps {
        let action = pick_action(&mut rng);
        if verbose && step % 500 == 0 {
            eprintln!(
                "seed={seed:#x} step={step} action={action:?} flows={} created={} evicted={}",
                mgr.flow_count(),
                model.created_seen,
                model.evicted_seen,
            );
        }

        match action {
            Action::ClientDatagram => {
                let src = pooled_source(&mut rng);
                let payload = random_payload(&mut rng, max_rx);
                mgr.handle_input(
                    ManagerInput::ClientDatagram {
                        src,
                        payload: &payload,
                    },
                    now,
                );
            }
            Action::BackendResolved => {
                // Valid path: resolve a flow we have a SelectBackend for.
                // Stale path: resolve a random id (UnknownFlow / late-resolve).
                let flow = if !model.awaiting.is_empty() && rng.random_bool(0.7) {
                    let i = rng.random_range(0..model.awaiting.len());
                    model.awaiting.swap_remove(i)
                } else {
                    rng.random_range(0..64usize)
                };
                let (backend, addr) = pooled_backend(&mut rng);
                mgr.handle_input(
                    ManagerInput::BackendResolved {
                        flow,
                        backend,
                        addr,
                    },
                    now,
                );
                // Remember it so backend datagrams have a plausible target. The
                // id may already be closed; the core must tolerate that.
                model.established.push(flow);
                if model.established.len() > 256 {
                    model.established.remove(0);
                }
            }
            Action::BackendDatagram => {
                let flow = if !model.established.is_empty() && rng.random_bool(0.7) {
                    let i = rng.random_range(0..model.established.len());
                    model.established[i]
                } else {
                    rng.random_range(0..64usize)
                };
                let payload = random_payload(&mut rng, max_rx);
                mgr.handle_input(
                    ManagerInput::BackendDatagram {
                        flow,
                        payload: &payload,
                    },
                    now,
                );
            }
            Action::ReconfigCluster => {
                let cfg = random_cluster(&mut rng, true);
                mgr.handle_input(ManagerInput::Config(ConfigEvent::SetCluster(cfg)), now);
            }
            Action::SetMaxFlows => {
                // Sometimes shrink BELOW the live count (legal, sheds future
                // flows, evicts none); sometimes grow above it.
                let n = if rng.random_bool(0.5) {
                    rng.random_range(0..=mgr.flow_count()) // possibly below live
                } else {
                    rng.random_range(1..64) // grow
                };
                model.cap_high_water = model.cap_high_water.max(n);
                mgr.handle_input(ManagerInput::Config(ConfigEvent::SetMaxFlows(n)), now);
            }
            Action::SetMaxRx => {
                max_rx = rng.random_range(16..4096);
                mgr.handle_input(
                    ManagerInput::Config(ConfigEvent::SetMaxRxDatagramSize(max_rx)),
                    now,
                );
            }
            Action::Drain => {
                // Drain is a one-way latch in the core (no resume): the shell
                // drains a listener, lets its flows reach teardown, then drops
                // the manager and stands up a fresh one. We model that whole
                // listener-lifecycle episode here so the long run keeps admitting
                // afterwards (otherwise every post-drain datagram is shed and the
                // rest of the run is dead coverage).
                mgr.handle_input(ManagerInput::Config(ConfigEvent::Drain), now);
                // New flows are now shed; a datagram during drain must allocate
                // nothing (Shed path).
                {
                    let src = pooled_source(&mut rng);
                    let payload = random_payload(&mut rng, max_rx);
                    mgr.handle_input(
                        ManagerInput::ClientDatagram {
                            src,
                            payload: &payload,
                        },
                        now,
                    );
                }
                // Push the clock past every idle deadline so the reaper drains
                // the surviving flows, then close any stragglers (e.g. ones whose
                // resolve is still pending). Verify drain-to-zero before rebuild.
                now += Duration::from_secs(30);
                mgr.handle_timeout(now);
                let _ = model.drain(&mut mgr);
                mgr.close_all(now);
                let _ = model.drain(&mut mgr);
                assert_eq!(
                    mgr.flow_count(),
                    0,
                    "seed={seed:#x} step={step} [drain-episode]: drain+reap did not reach zero",
                );
                assert_eq!(
                    model.created_seen, model.evicted_seen,
                    "seed={seed:#x} step={step} [drain-episode]: created {} != evicted {} after drain",
                    model.created_seen, model.evicted_seen,
                );
                // Stand up a fresh listener (the shell's post-drain rebuild).
                let cfg = random_cluster(&mut rng, false);
                let cap = rng.random_range(1..32usize);
                max_rx = 1500;
                mgr = UdpManager::new(cfg, cap, max_rx, hash_seed);
                model.cap_high_water = model.cap_high_water.max(cap);
                model.awaiting.clear();
                model.established.clear();
            }
            Action::AdvanceClock => {
                // Mostly small jumps (flows survive, exercising steady state);
                // occasionally a large jump to force mass idle reaping. Deltas in
                // ms..several seconds; the large jump exceeds the timeout floor.
                let delta_ms = if rng.random_bool(0.12) {
                    rng.random_range(5_000..15_000)
                } else {
                    rng.random_range(1..300)
                };
                now += Duration::from_millis(delta_ms);
                mgr.handle_timeout(now);
            }
            Action::AbortFlow => {
                let flow = rng.random_range(0..64usize);
                mgr.abort_flow(flow, now, CloseReason::Aborted);
            }
            Action::CloseAll => {
                // close_all does NOT latch draining, so admission continues on
                // the same manager — no rebuild needed. Every live flow is torn
                // down; the awaiting pool is now stale. (We keep `established`:
                // those ids may be reused by the slab, and feeding stale ids back
                // in exercises the UnknownFlow path, which is the point.)
                mgr.close_all(now);
                model.awaiting.clear();
            }
        }

        // Drain outputs, fold into the model, and honour a subset of the new
        // SelectBackend requests by replying BackendResolved so flows progress
        // to Established. Resolving a subset (not all) keeps some flows parked
        // AwaitingBackend, which is itself a state worth exercising.
        let selects = model.drain(&mut mgr);
        for (flow, _key) in selects {
            if rng.random_bool(0.6) {
                let (backend, addr) = pooled_backend(&mut rng);
                mgr.handle_input(
                    ManagerInput::BackendResolved {
                        flow,
                        backend,
                        addr,
                    },
                    now,
                );
                model.established.push(flow);
                let after = model.drain(&mut mgr);
                // A resolve can itself surface nothing new (no further selects),
                // but fold whatever it produced.
                model.awaiting.extend(after.into_iter().map(|(f, _)| f));
            } else {
                model.awaiting.push(flow);
            }
        }
        if model.established.len() > 256 {
            let overflow = model.established.len() - 256;
            model.established.drain(0..overflow);
        }

        // Buggify: low-probability extra adversarial events.
        if buggify(&mut rng, 0.02) {
            match rng.random_range(0..4u8) {
                // Stale BackendResolved for a (likely closed) id.
                0 => {
                    let flow = rng.random_range(0..128usize);
                    let (backend, addr) = pooled_backend(&mut rng);
                    mgr.handle_input(
                        ManagerInput::BackendResolved {
                            flow,
                            backend,
                            addr,
                        },
                        now,
                    );
                }
                // Reconfig burst.
                1 => {
                    for _ in 0..rng.random_range(2..6u8) {
                        let cfg = random_cluster(&mut rng, true);
                        mgr.handle_input(ManagerInput::Config(ConfigEvent::SetCluster(cfg)), now);
                    }
                }
                // Shrink max_flows to a tiny value.
                2 => {
                    let n = rng.random_range(0..2usize);
                    model.cap_high_water = model.cap_high_water.max(n);
                    mgr.handle_input(ManagerInput::Config(ConfigEvent::SetMaxFlows(n)), now);
                }
                // Giant clock jump.
                _ => {
                    now += Duration::from_secs(rng.random_range(30..600));
                    mgr.handle_timeout(now);
                }
            }
        }

        // Drain everything the buggify/extra resolves produced, then check.
        let _ = model.drain(&mut mgr);
        model.check(&mut mgr, seed, step, "step");
    }

    // --------------------------------------------------------------------
    // FINAL invariant: a giant clock jump + close_all must drain to zero.
    // Proves the reaper + teardown are complete (no flow / slab leak).
    // --------------------------------------------------------------------
    now += Duration::from_secs(3600);
    mgr.handle_timeout(now);
    let _ = model.drain(&mut mgr);

    mgr.close_all(now);
    let _ = model.drain(&mut mgr);

    assert_eq!(
        mgr.flow_count(),
        0,
        "seed={seed:#x}: FINAL close_all left {} live flows",
        mgr.flow_count(),
    );
    assert!(
        mgr.poll_timeout().is_none(),
        "seed={seed:#x}: FINAL close_all left an armed timer {:?}",
        mgr.poll_timeout(),
    );
    assert_eq!(
        model.created_seen, model.evicted_seen,
        "seed={seed:#x}: FINAL created {} != evicted {} (flow/slab leak)",
        model.created_seen, model.evicted_seen,
    );
    if verbose {
        eprintln!(
            "seed={seed:#x} DONE steps={steps} created={} evicted={} shed={} peak_flows={}",
            model.created_seen, model.evicted_seen, model.shed_seen, model.peak_flows,
        );
    }
}

// --------------------------------------------------------------------------
// Env-knob parsing (decimal or 0x-hex).
// --------------------------------------------------------------------------

/// Parse a `u64` env var accepting both decimal and `0x`-prefixed hex.
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

// --------------------------------------------------------------------------
// Tests.
// --------------------------------------------------------------------------

/// Deterministic seed sweep (FoundationDB nightly seed-sweep / VOPR analog).
///
/// Runs seeds `0..256` by default, each a few-thousand-step adversarial run.
/// Kept fast (pure core, no I/O) so it rides along in `cargo test --workspace`.
/// On any failure the panic message carries the failing seed + step, so the
/// exact run reproduces via `SOZU_UDP_SIM_SEED=<seed>`.
///
/// Env overrides:
/// - `SOZU_UDP_SIM_SEED=<u64|0xhex>`: run that ONE seed with verbose tracing.
/// - `SOZU_UDP_SIM_SEEDS=<n>`: sweep `0..n` seeds instead of `0..256`.
/// - `SOZU_UDP_SIM_STEPS=<n>`: run `n` steps per seed instead of the default.
#[test]
fn udp_simulation_seed_sweep() {
    let steps = env_usize("SOZU_UDP_SIM_STEPS").unwrap_or(3_000);

    // Single-seed replay mode: reproduce a CI failure verbosely.
    if let Some(seed) = env_u64("SOZU_UDP_SIM_SEED") {
        eprintln!("== UDP sim single-seed replay: seed={seed:#x} steps={steps} ==");
        run_simulation(seed, steps, true);
        return;
    }

    let seeds = env_u64("SOZU_UDP_SIM_SEEDS").unwrap_or(256);
    let started = Instant::now();
    for seed in 0..seeds {
        run_simulation(seed, steps, false);
    }
    eprintln!(
        "UDP sim sweep OK: {seeds} seeds × {steps} steps in {:?}",
        started.elapsed(),
    );
}

/// Fast end-to-end smoke test pinning one representative seed. Catches gross
/// regressions in a single fast run without the full sweep, and serves as a
/// living example of the harness driving a complete simulation.
#[test]
fn udp_simulation_replays_known_seed() {
    // A fixed, representative seed; deep enough to exercise reconfig + reaping +
    // teardown but fast on its own.
    run_simulation(0x5E_ED_C0_DE, 4_000, false);
}

/// Determinism guard: the same seed produces an identical observable trace
/// (created / evicted / shed tallies). If this ever diverges, the core gained a
/// hidden nondeterministic dependency (wall clock, rand, hash-map iteration
/// leaking into outputs) — which would break replay.
#[test]
fn udp_simulation_is_deterministic() {
    fn fingerprint(seed: u64, steps: usize) -> (u64, u64, u64) {
        let mut rng = StdRng::seed_from_u64(seed);
        let base = Instant::now();
        let mut now = base;
        let cap = rng.random_range(1..32usize);
        let cluster = random_cluster(&mut rng, false);
        let hash_seed: u64 = rng.random();
        let mut mgr = UdpManager::new(cluster, cap, 1500, hash_seed);
        let mut model = Model::new(cap);
        let _ = model.drain(&mut mgr);
        for _ in 1..=steps {
            // Mirror only the ClientDatagram + AdvanceClock subset; enough to
            // produce a non-trivial, reproducible tally without re-deriving the
            // full driver. Determinism of the full driver is covered by the
            // sweep reproducing from a seed.
            if rng.random_bool(0.7) {
                let src = pooled_source(&mut rng);
                let payload = random_payload(&mut rng, 1500);
                mgr.handle_input(
                    ManagerInput::ClientDatagram {
                        src,
                        payload: &payload,
                    },
                    now,
                );
                let selects = model.drain(&mut mgr);
                for (flow, _) in selects {
                    let (backend, addr) = pooled_backend(&mut rng);
                    mgr.handle_input(
                        ManagerInput::BackendResolved {
                            flow,
                            backend,
                            addr,
                        },
                        now,
                    );
                }
            } else {
                now += Duration::from_millis(rng.random_range(1..3000));
                mgr.handle_timeout(now);
            }
            let _ = model.drain(&mut mgr);
        }
        now += Duration::from_secs(3600);
        mgr.handle_timeout(now);
        let _ = model.drain(&mut mgr);
        mgr.close_all(now);
        let _ = model.drain(&mut mgr);
        (model.created_seen, model.evicted_seen, model.shed_seen)
    }

    let a = fingerprint(0xABCDEF, 1_500);
    let b = fingerprint(0xABCDEF, 1_500);
    assert_eq!(
        a, b,
        "same seed must yield an identical trace (determinism)"
    );
    assert_eq!(
        a.0, a.1,
        "final close_all must evict everything created (no leak)"
    );
}

/// Sanity: env-knob parsing accepts both decimal and 0x-hex.
#[test]
fn env_parse_accepts_hex_and_decimal() {
    assert_eq!(parse_u64("42"), Some(42));
    assert_eq!(parse_u64(" 256 "), Some(256));
    assert_eq!(parse_u64("0xdeadbeef"), Some(0xdead_beef));
    assert_eq!(parse_u64("0XFF"), Some(0xFF));
    assert_eq!(parse_u64("notanumber"), None);
}
