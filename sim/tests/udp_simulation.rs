// Gated on `--cfg tokio_unstable`: moonpool-sim seeds tokio's runtime RNG via the
// unstable `RngSeed` API. Without the flag this whole test crate compiles to an
// empty (0-test) binary and pulls no moonpool/tokio deps (see Cargo.toml), so a
// plain `cargo test --workspace` stays free of tokio_unstable. Run the real sweep
// with `RUSTFLAGS="--cfg tokio_unstable" cargo test -p sozu-sim`.
#![cfg(tokio_unstable)]
//! Deterministic simulation of the sans-io UDP core (`sozu_lib::protocol::udp`)
//! driven by the [moonpool-sim] engine.
//!
//! This is the sole UDP deterministic-sim, driven by moonpool-sim; it replaced
//! an earlier handmade synchronous harness. moonpool supplies the seeded runtime,
//! the virtual clock ([`SimContext::time`]), the seeded RNG ([`SimContext::random`])
//! and the `buggify` fault-injection vocabulary; this file supplies the
//! adversarial workload, the shadow model, and the cross-step invariants that
//! drive [`UdpManager`] hard across many seeds.
//!
//! `lib/` stays async-free — this crate is the only async home for the UDP
//! simulation (moonpool/tokio are dev-dependencies, gated on `tokio_unstable`).
//!
//! # Why a `std::time::Instant` base
//!
//! moonpool's clock yields a [`Duration`] since simulation start; the UDP core
//! takes a `std::time::Instant`. A base `Instant` is captured once per seed and
//! the simulated `Duration` is added to it. The core only ever compares
//! `Instant`s relatively, so the absolute base is irrelevant and observable
//! outputs stay a pure function of the seed.
//!
//! # Swarm configurations (Groce et al., "Swarm Testing", ISSTA 2012)
//!
//! Each seed draws a [`SwarmConfig`] from the seeded RNG BEFORE the first
//! operation: a random subset of the OPTIONAL/SUPPRESSOR grammar features
//! (50% inclusion each), with the MANDATORY `ClientDatagram` always retained
//! and the remaining weights renormalized. Omitting a SUPPRESSOR (`Drain`,
//! `CloseAll`, `AbortFlow`, `SetMaxFlows` — each repairs or prevents the very
//! full-table state a capacity bug needs) lets a seed drive the manager into
//! states the all-features grammar repairs too eagerly; omitting other
//! features concentrates the step budget on the survivors. Seeds divisible
//! by four keep the inclusive all-features configuration (the paper is
//! explicit that swarm subsets complement, never replace, it: a bug needing
//! `k` features together appears in a coin-toss subset with probability
//! `1/2^k`); campaign seeds are fixed to `0..n`, which reserves exactly one
//! inclusive run in each four-seed cohort. Degenerate all-off and all-on subset
//! draws are repaired to
//! non-empty proper subsets. Each buggify arm is
//! gated by its sibling grammar feature and skipped (never redrawn) when that
//! feature is disabled. The drawn configuration is a pure function of the
//! seed and is printed as one canonical `swarm-config` line before the
//! workload runs, so any failing seed's configuration is always in the
//! captured output. `SOZU_SIM_SWARM=0` disables the draw entirely (zero extra
//! RNG consumption — byte-identical to the historical all-features grammar)
//! for direct swarm-vs-inclusive campaign comparison.
//!
//! # Replay / sweep ergonomics
//!
//! - `SOZU_UDP_SIM_SEED=<u64|0xhex>` — replay that ONE seed verbosely.
//! - `SOZU_UDP_SIM_SEEDS=<n>` — sweep `n` iterations (default 256).
//! - `SOZU_UDP_SIM_STEPS=<n>` — steps per seed (default 3000).
//! - `SOZU_SIM_SWARM=0|1` — draw per-seed swarm configurations (default `1`;
//!   `0` pins every seed to the historical all-features configuration).
//!
//! On a hard invariant violation the panic carries the failing seed (via
//! `current_sim_seed`) + step; moonpool also records it in the report's
//! `seeds_failing`. (Seed values are moonpool's, not portable from the former
//! handmade harness — the RNG engine differs.)
//!
//! [moonpool-sim]: https://crates.io/crates/moonpool-sim

use std::{
    net::{IpAddr, Ipv4Addr, SocketAddr},
    sync::{Arc, Mutex},
    time::{Duration, Instant},
};

use async_trait::async_trait;
use moonpool_sim::{
    RandomProvider, SimContext, SimulationBuilder, SimulationReport, SimulationResult,
    TimeProvider, Workload, buggify_with_prob, current_sim_seed,
};
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
fn pooled_source(ctx: &SimContext) -> SocketAddr {
    let ip = ctx.random().random_range(0..POOL_IPS);
    let port = POOL_PORT_BASE + ctx.random().random_range(0..POOL_PORTS);
    SocketAddr::new(IpAddr::V4(Ipv4Addr::new(10, 0, 0, ip)), port)
}

/// A small set of distinct backend addresses for resolution replies.
fn pooled_backend(ctx: &SimContext) -> (String, SocketAddr) {
    let n = ctx.random().random_range(0..4u8);
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
    created_seen: u64,
    evicted_seen: u64,
    awaiting: Vec<FlowId>,
    established: Vec<FlowId>,
    cap_high_water: usize,
    shed_seen: u64,
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
    /// shadow model. Returns the `SelectBackend` flow ids surfaced this drain.
    fn drain(&mut self, mgr: &mut UdpManager) -> Vec<(FlowId, u64)> {
        let mut selects = Vec::new();
        while let Some(out) = mgr.poll_output() {
            match out {
                Output::Metric(MetricEvent::FlowCreated) => self.created_seen += 1,
                Output::Metric(MetricEvent::FlowEvicted) => self.evicted_seen += 1,
                Output::Metric(MetricEvent::FlowShed) => self.shed_seen += 1,
                Output::SelectBackend { flow, key, .. } => selects.push((flow, key)),
                _ => {}
            }
        }
        selects
    }

    /// Harness-level invariants, checked after every fully-drained step. The
    /// core's own `debug_assert` invariants fire for free during each
    /// `handle_*` call; these add the cross-step model checks.
    fn check(&mut self, mgr: &mut UdpManager, step: usize, ctx_label: &str) {
        let seed = current_sim_seed();
        self.peak_flows = self.peak_flows.max(mgr.flow_count());

        // (a) cap high-water bound.
        assert!(
            mgr.flow_count() <= self.cap_high_water,
            "seed={seed:#x} step={step} [{ctx_label}]: flow_count {} exceeds cap high-water {}",
            mgr.flow_count(),
            self.cap_high_water,
        );

        // (b) the queue is fully drained before this check runs.
        assert!(
            mgr.poll_output().is_none(),
            "seed={seed:#x} step={step} [{ctx_label}]: output queue not drained to None",
        );

        // (c) active-flow accounting balances with no underflow / leak.
        assert!(
            self.created_seen >= self.evicted_seen,
            "seed={seed:#x} step={step} [{ctx_label}]: evicted {} > created {} (gauge underflow)",
            self.evicted_seen,
            self.created_seen,
        );
        let active = self.created_seen - self.evicted_seen;
        assert_eq!(
            active as usize,
            mgr.flow_count(),
            "seed={seed:#x} step={step} [{ctx_label}]: model balance created-evicted={active} \
             != flow_count {}",
            mgr.flow_count(),
        );

        // (d) timer coherence at the harness level.
        assert_eq!(
            mgr.poll_timeout().is_some(),
            mgr.flow_count() > 0,
            "seed={seed:#x} step={step} [{ctx_label}]: timer-coherence armed={:?} but flow_count={}",
            mgr.poll_timeout(),
            mgr.flow_count(),
        );
    }
}

// --------------------------------------------------------------------------
// Workload grammar.
// --------------------------------------------------------------------------

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Action {
    ClientDatagram,
    BackendDatagram,
    BackendResolved,
    ReconfigCluster,
    SetMaxFlows,
    SetMaxRx,
    Drain,
    AdvanceClock,
    AbortFlow,
    CloseAll,
}

/// The weighted action grammar (weights sum to 100), in the EXACT dispatch
/// order of the pre-swarm `0..100` `match`: a cumulative walk over this table
/// with every feature enabled maps each roll to the same action the
/// historical grammar did, so an all-features configuration stays
/// draw-identical to the pre-swarm harness.
///
/// Swarm classification (see `doc/testing.md`): index 0 (`ClientDatagram`) is
/// MANDATORY — it is the sole flow creator, and without it every shadow-model
/// invariant is vacuously green. `Drain`, `CloseAll`, `AbortFlow`, and
/// `SetMaxFlows` are SUPPRESSORS — each evicts flows, resets the manager, or
/// sheds future admissions, repairing or preventing the very full-table state
/// a capacity bug needs. The rest are OPTIONAL grammar features.
const ACTION_TABLE: [(Action, u32); 10] = [
    (Action::ClientDatagram, 34),
    (Action::BackendResolved, 16),
    (Action::BackendDatagram, 14),
    (Action::AdvanceClock, 14),
    (Action::ReconfigCluster, 6),
    (Action::SetMaxFlows, 5),
    (Action::AbortFlow, 4),
    (Action::SetMaxRx, 3),
    (Action::Drain, 2),
    (Action::CloseAll, 2),
];

/// Per-seed swarm configuration: which grammar features this seed may draw.
/// `enabled[i]` mirrors `ACTION_TABLE[i]`.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct SwarmConfig {
    enabled: [bool; 10],
}

/// `SOZU_SIM_SWARM=0` disables per-seed swarm draws (every seed then runs the
/// historical all-features configuration with zero extra RNG consumption);
/// unset or any other value leaves swarm on.
fn swarm_enabled() -> bool {
    !matches!(std::env::var("SOZU_SIM_SWARM"), Ok(v) if v.trim() == "0")
}

fn inclusive_seed(seed: u64) -> bool {
    seed.is_multiple_of(4)
}

fn campaign_seeds(count: usize) -> Vec<u64> {
    (0..u64::try_from(count).expect("campaign seed count fits in u64")).collect()
}

impl SwarmConfig {
    /// The inclusive all-features configuration (`C_D` in the paper).
    fn full() -> Self {
        SwarmConfig {
            enabled: [true; 10],
        }
    }

    /// Draw this seed's configuration from the seeded RNG. Seeds divisible by
    /// four keep the inclusive configuration; the rest coin-toss each
    /// OPTIONAL/SUPPRESSOR feature at 50%, always retain the MANDATORY
    /// `ClientDatagram`, and repair all-off/all-on draws to proper subsets.
    fn draw(ctx: &SimContext, seed: u64) -> Self {
        if inclusive_seed(seed) {
            return SwarmConfig::full();
        }
        let mut enabled = [false; 10];
        enabled[0] = true; // ClientDatagram is MANDATORY.
        for slot in enabled.iter_mut().skip(1) {
            *slot = ctx.random().random_bool(0.5);
        }
        if !enabled.iter().skip(1).any(|e| *e) {
            enabled[1] = true;
        } else if enabled.iter().all(|e| *e) {
            enabled[9] = false;
        }
        let cfg = SwarmConfig { enabled };
        debug_assert!(cfg.enabled[0], "the MANDATORY feature must stay enabled");
        debug_assert!(
            cfg.total_weight() > ACTION_TABLE[0].1,
            "a drawn subset keeps at least one optional feature"
        );
        debug_assert!(
            !cfg.enabled.iter().all(|e| *e),
            "a non-reserved seed must use a proper subset"
        );
        cfg
    }

    fn is_enabled(&self, action: Action) -> bool {
        ACTION_TABLE
            .iter()
            .zip(&self.enabled)
            .find(|((a, _), _)| *a == action)
            .is_some_and(|(_, e)| *e)
    }

    /// Renormalized total weight over the enabled features.
    fn total_weight(&self) -> u32 {
        ACTION_TABLE
            .iter()
            .zip(&self.enabled)
            .filter(|(_, e)| **e)
            .map(|((_, w), _)| *w)
            .sum()
    }

    /// The canonical one-line configuration record, printed before the
    /// workload runs. Byte-identical across replays of the same seed — a
    /// failing seed whose configuration is not printed is not reproducible.
    fn log_line(&self, seed: u64, swarm: bool) -> String {
        let mode = if !swarm {
            "off"
        } else if self.enabled.iter().all(|e| *e) {
            "full"
        } else {
            "subset"
        };
        let features: Vec<String> = ACTION_TABLE
            .iter()
            .zip(&self.enabled)
            .filter(|(_, e)| **e)
            .map(|((a, w), _)| format!("{a:?}:{w}"))
            .collect();
        format!(
            "swarm-config sim=udp seed={seed:#x} mode={mode} features=[{}] total_weight={}",
            features.join(","),
            self.total_weight(),
        )
    }
}

/// Draw a weighted-random action among the configuration's enabled features:
/// exactly ONE `random_range` draw over the renormalized total, then a
/// cumulative walk in `ACTION_TABLE` order. With every feature enabled this
/// consumes the same single draw and maps rolls to actions exactly as the
/// historical `0..100` `match` did.
fn pick_action(ctx: &SimContext, cfg: &SwarmConfig) -> Action {
    let roll = ctx.random().random_range(0..cfg.total_weight());
    let mut remaining = roll;
    for ((action, weight), enabled) in ACTION_TABLE.iter().zip(&cfg.enabled) {
        if !enabled {
            continue;
        }
        if remaining < *weight {
            return *action;
        }
        remaining -= weight;
    }
    unreachable!("roll {roll} below the renormalized total always lands on an enabled action")
}

/// A randomized cluster config (non-empty cluster name unless `allow_empty`).
fn random_cluster(ctx: &SimContext, allow_empty: bool) -> ClusterConfig {
    let cluster = if allow_empty && ctx.random().random_bool(0.1) {
        String::new()
    } else {
        format!("cluster-{}", ctx.random().random_range(0..3u8))
    };
    ClusterConfig {
        cluster,
        affinity_with_port: ctx.random().random_bool(0.5),
        responses: if ctx.random().random_bool(0.6) {
            0
        } else {
            ctx.random().random_range(1..4u32)
        },
        requests: if ctx.random().random_bool(0.6) {
            0
        } else {
            ctx.random().random_range(1..6u32)
        },
        front_timeout: Duration::from_millis(ctx.random().random_range(500..5000)),
        back_timeout: Duration::from_millis(ctx.random().random_range(500..5000)),
        send_proxy_protocol: ctx.random().random_bool(0.3),
        proxy_protocol_every_datagram: ctx.random().random_bool(0.5),
    }
}

/// A random payload sized relative to `max_rx`: empty / tiny / typical /
/// oversized (`> max_rx`, exercising the Truncated drop path).
fn random_payload(ctx: &SimContext, max_rx: usize) -> Vec<u8> {
    let over = max_rx.saturating_add(1).max(2);
    let len = match ctx.random().random_range(0..16u32) {
        0 => 0,
        1..3 => ctx.random().random_range(1..16),
        // moonpool's `random_range` takes an exclusive `Range`, so `..=hi`
        // becomes `..hi + 1`.
        3..13 => ctx.random().random_range(1..max_rx.max(1) + 1).min(512),
        _ => ctx.random().random_range(over..over + 2049),
    };
    let byte: u8 = ctx.random().random();
    vec![byte; len]
}

// --------------------------------------------------------------------------
// The workload: one moonpool iteration == one deterministic seed run.
// --------------------------------------------------------------------------

/// Optional sink for the determinism guard: the final `(created, evicted, shed)`
/// tally is pushed here so two runs of the same seed can be compared.
type FingerprintSink = Arc<Mutex<Vec<(u64, u64, u64)>>>;
/// Optional sink for the swarm-config stability guard: the configuration
/// drawn for each seed is pushed here so two runs can be compared.
type ConfigSink = Arc<Mutex<Vec<SwarmConfig>>>;

struct UdpSimWorkload {
    steps: usize,
    verbose: bool,
    sink: Option<FingerprintSink>,
    /// `true` draws a per-seed [`SwarmConfig`]; `false` pins the historical
    /// all-features configuration with zero extra RNG consumption.
    swarm: bool,
    config_sink: Option<ConfigSink>,
}

#[async_trait]
impl Workload for UdpSimWorkload {
    fn name(&self) -> &'static str {
        "udp_simulation"
    }

    async fn run(&mut self, ctx: &SimContext) -> SimulationResult<()> {
        let seed = current_sim_seed();
        // Virtual clock base: captured once; the simulated Duration is added to
        // it so the core sees monotonically advancing `Instant`s.
        let base = Instant::now();
        let now = |ctx: &SimContext| base + ctx.time().now();

        // Swarm configuration: drawn from the seeded RNG BEFORE the first
        // operation (a pure function of the seed), and printed before the
        // workload runs so a failing seed's configuration is always in the
        // captured output. With swarm off, no RNG draw happens at all — the
        // run is byte-identical to the historical all-features grammar.
        let swarm_cfg = if self.swarm {
            SwarmConfig::draw(ctx, seed)
        } else {
            SwarmConfig::full()
        };
        eprintln!("{}", swarm_cfg.log_line(seed, self.swarm));
        if let Some(sink) = &self.config_sink {
            sink.lock().unwrap().push(swarm_cfg);
        }

        let initial_cap = ctx.random().random_range(1..32usize);
        let initial_max_rx = 1500usize;
        let cluster = random_cluster(ctx, false);
        let hash_seed: u64 = ctx.random().random();
        let mut mgr = UdpManager::new(cluster, initial_cap, initial_max_rx, hash_seed);
        let mut model = Model::new(initial_cap);
        let mut max_rx = initial_max_rx;

        let _ = model.drain(&mut mgr);
        model.check(&mut mgr, 0, "init");

        for step in 1..=self.steps {
            let action = pick_action(ctx, &swarm_cfg);
            if self.verbose && step % 500 == 0 {
                eprintln!(
                    "seed={seed:#x} step={step} action={action:?} flows={} created={} evicted={}",
                    mgr.flow_count(),
                    model.created_seen,
                    model.evicted_seen,
                );
            }

            match action {
                Action::ClientDatagram => {
                    let src = pooled_source(ctx);
                    let payload = random_payload(ctx, max_rx);
                    mgr.handle_input(
                        ManagerInput::ClientDatagram {
                            src,
                            payload: &payload,
                        },
                        now(ctx),
                    );
                }
                Action::BackendResolved => {
                    let flow = if !model.awaiting.is_empty() && ctx.random().random_bool(0.7) {
                        let i = ctx.random().random_range(0..model.awaiting.len());
                        model.awaiting.swap_remove(i)
                    } else {
                        ctx.random().random_range(0..64usize)
                    };
                    let (backend, addr) = pooled_backend(ctx);
                    mgr.handle_input(
                        ManagerInput::BackendResolved {
                            flow,
                            backend,
                            addr,
                        },
                        now(ctx),
                    );
                    model.established.push(flow);
                    if model.established.len() > 256 {
                        model.established.remove(0);
                    }
                }
                Action::BackendDatagram => {
                    let flow = if !model.established.is_empty() && ctx.random().random_bool(0.7) {
                        let i = ctx.random().random_range(0..model.established.len());
                        model.established[i]
                    } else {
                        ctx.random().random_range(0..64usize)
                    };
                    let payload = random_payload(ctx, max_rx);
                    mgr.handle_input(
                        ManagerInput::BackendDatagram {
                            flow,
                            payload: &payload,
                        },
                        now(ctx),
                    );
                }
                Action::ReconfigCluster => {
                    let cfg = random_cluster(ctx, true);
                    mgr.handle_input(ManagerInput::Config(ConfigEvent::SetCluster(cfg)), now(ctx));
                }
                Action::SetMaxFlows => {
                    let n = if ctx.random().random_bool(0.5) {
                        ctx.random().random_range(0..mgr.flow_count() + 1)
                    } else {
                        ctx.random().random_range(1..64)
                    };
                    model.cap_high_water = model.cap_high_water.max(n);
                    mgr.handle_input(ManagerInput::Config(ConfigEvent::SetMaxFlows(n)), now(ctx));
                }
                Action::SetMaxRx => {
                    max_rx = ctx.random().random_range(16..4096);
                    mgr.handle_input(
                        ManagerInput::Config(ConfigEvent::SetMaxRxDatagramSize(max_rx)),
                        now(ctx),
                    );
                }
                Action::Drain => {
                    // Drain is a one-way latch: drain, shed, reap, verify
                    // drain-to-zero, then stand up a fresh listener.
                    mgr.handle_input(ManagerInput::Config(ConfigEvent::Drain), now(ctx));
                    {
                        let src = pooled_source(ctx);
                        let payload = random_payload(ctx, max_rx);
                        mgr.handle_input(
                            ManagerInput::ClientDatagram {
                                src,
                                payload: &payload,
                            },
                            now(ctx),
                        );
                    }
                    // Push the virtual clock past every idle deadline.
                    let _ = ctx.time().sleep(Duration::from_secs(30)).await;
                    mgr.handle_timeout(now(ctx));
                    let _ = model.drain(&mut mgr);
                    mgr.close_all(now(ctx));
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
                    let cfg = random_cluster(ctx, false);
                    let cap = ctx.random().random_range(1..32usize);
                    max_rx = 1500;
                    mgr = UdpManager::new(cfg, cap, max_rx, hash_seed);
                    model.cap_high_water = model.cap_high_water.max(cap);
                    model.awaiting.clear();
                    model.established.clear();
                }
                Action::AdvanceClock => {
                    let delta_ms = if ctx.random().random_bool(0.12) {
                        ctx.random().random_range(5_000..15_000)
                    } else {
                        ctx.random().random_range(1..300)
                    };
                    let _ = ctx.time().sleep(Duration::from_millis(delta_ms)).await;
                    mgr.handle_timeout(now(ctx));
                }
                Action::AbortFlow => {
                    let flow = ctx.random().random_range(0..64usize);
                    mgr.abort_flow(flow, now(ctx), CloseReason::Aborted);
                }
                Action::CloseAll => {
                    mgr.close_all(now(ctx));
                    model.awaiting.clear();
                }
            }

            // Drain outputs, fold into the model, honour a subset of new
            // SelectBackend requests so flows progress to Established.
            let selects = model.drain(&mut mgr);
            for (flow, _key) in selects {
                if ctx.random().random_bool(0.6) {
                    let (backend, addr) = pooled_backend(ctx);
                    mgr.handle_input(
                        ManagerInput::BackendResolved {
                            flow,
                            backend,
                            addr,
                        },
                        now(ctx),
                    );
                    model.established.push(flow);
                    let after = model.drain(&mut mgr);
                    model.awaiting.extend(after.into_iter().map(|(f, _)| f));
                } else {
                    model.awaiting.push(flow);
                }
            }
            if model.established.len() > 256 {
                let overflow = model.established.len() - 256;
                model.established.drain(0..overflow);
            }

            // Buggify: low-probability extra adversarial events, using
            // moonpool's fault-injection primitive. Each arm is gated by its
            // sibling grammar feature so a swarm subset omits the fault along
            // with the feature; a disabled arm is skipped (never redrawn),
            // keeping the draw sequence a pure function of (seed, config).
            if buggify_with_prob!(0.02) {
                let arm = ctx.random().random_range(0..4u8);
                let arm_feature = match arm {
                    0 => Action::BackendResolved,
                    1 => Action::ReconfigCluster,
                    2 => Action::SetMaxFlows,
                    _ => Action::AdvanceClock,
                };
                if swarm_cfg.is_enabled(arm_feature) {
                    match arm {
                        0 => {
                            let flow = ctx.random().random_range(0..128usize);
                            let (backend, addr) = pooled_backend(ctx);
                            mgr.handle_input(
                                ManagerInput::BackendResolved {
                                    flow,
                                    backend,
                                    addr,
                                },
                                now(ctx),
                            );
                        }
                        1 => {
                            for _ in 0..ctx.random().random_range(2..6u8) {
                                let cfg = random_cluster(ctx, true);
                                mgr.handle_input(
                                    ManagerInput::Config(ConfigEvent::SetCluster(cfg)),
                                    now(ctx),
                                );
                            }
                        }
                        2 => {
                            let n = ctx.random().random_range(0..2usize);
                            model.cap_high_water = model.cap_high_water.max(n);
                            mgr.handle_input(
                                ManagerInput::Config(ConfigEvent::SetMaxFlows(n)),
                                now(ctx),
                            );
                        }
                        _ => {
                            // Giant jump relative to the 5s max idle timeout (so it
                            // mass-reaps), but bounded so the run stays within
                            // moonpool's simulated-time budget over a deep sweep.
                            let _ = ctx
                                .time()
                                .sleep(Duration::from_secs(ctx.random().random_range(10..60)))
                                .await;
                            mgr.handle_timeout(now(ctx));
                        }
                    }
                }
            }

            let _ = model.drain(&mut mgr);
            model.check(&mut mgr, step, "step");
        }

        // FINAL invariant: a clock jump well past every idle deadline (the max
        // idle timeout is 5s) + close_all must drain to zero.
        let _ = ctx.time().sleep(Duration::from_secs(60)).await;
        mgr.handle_timeout(now(ctx));
        let _ = model.drain(&mut mgr);
        mgr.close_all(now(ctx));
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

        if self.verbose {
            eprintln!(
                "seed={seed:#x} DONE steps={} created={} evicted={} shed={} peak_flows={}",
                self.steps,
                model.created_seen,
                model.evicted_seen,
                model.shed_seen,
                model.peak_flows,
            );
        }
        if let Some(sink) = &self.sink {
            sink.lock()
                .unwrap()
                .push((model.created_seen, model.evicted_seen, model.shed_seen));
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

/// moonpool aborts a seed whose simulated time exceeds its run-time budget
/// (default 1 hour). This workload advances logical time by up to ~90s per step
/// (a drain or big clock jump plus a buggify giant jump), so a deep sweep
/// legitimately spans many hours of *simulated* time. Budget generously and
/// proportionally to the step count so a long-but-healthy run is never mistaken
/// for a stall. This is logical time only — moonpool runs it in milliseconds.
fn run_budget(steps: usize) -> Duration {
    Duration::from_secs((steps as u64).saturating_mul(150).saturating_add(3_600))
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

/// Deterministic seed sweep (FoundationDB nightly seed-sweep / VOPR analog).
/// On a hard invariant violation the panic carries the failing seed + step;
/// moonpool also lists it in `report.seeds_failing`.
#[test]
fn udp_simulation_seed_sweep() {
    let steps = env_usize("SOZU_UDP_SIM_STEPS").unwrap_or(3_000);
    let swarm = swarm_enabled();

    // Single-seed replay mode.
    if let Some(seed) = env_u64("SOZU_UDP_SIM_SEED") {
        eprintln!("== UDP sim single-seed replay: seed={seed:#x} steps={steps} swarm={swarm} ==");
        let report = SimulationBuilder::new()
            .workload(UdpSimWorkload {
                steps,
                verbose: true,
                sink: None,
                swarm,
                config_sink: None,
            })
            .set_debug_seeds(vec![seed])
            // Without an explicit iteration count, `SimulationBuilder`
            // defaults to `UntilCoverageStable` (up to 1000 iterations) and
            // would keep drawing FRESH random seeds after this one — fixing
            // it to 1 is what actually makes this "replay that ONE seed",
            // matching the documented `SOZU_UDP_SIM_SEED` contract (the
            // TCP preread sim's replay path carries the same fix).
            .set_iterations(1)
            .run_time_budget(run_budget(steps))
            .run();
        assert_no_failures(&report);
        return;
    }

    let seeds = env_usize("SOZU_UDP_SIM_SEEDS").unwrap_or(256);
    let report = SimulationBuilder::new()
        .workload(UdpSimWorkload {
            steps,
            verbose: false,
            sink: None,
            swarm,
            config_sink: None,
        })
        .set_debug_seeds(campaign_seeds(seeds))
        .set_iterations(seeds)
        .run_time_budget(run_budget(steps))
        .run();
    assert_no_failures(&report);
}

/// Fast smoke test pinning one representative seed. Swarm is off so the
/// pinned trajectory stays byte-identical to the pre-swarm harness (no
/// config draw touches the RNG stream) and the smoke test keeps exercising
/// the full grammar.
#[test]
fn udp_simulation_replays_known_seed() {
    let steps = 4_000;
    let report = SimulationBuilder::new()
        .workload(UdpSimWorkload {
            steps,
            verbose: false,
            sink: None,
            swarm: false,
            config_sink: None,
        })
        .set_debug_seeds(vec![0x5E_ED_C0_DE])
        // Pin the run to exactly this seed (see the replay-path comment).
        .set_iterations(1)
        .run_time_budget(run_budget(steps))
        .run();
    assert_no_failures(&report);
}

/// Determinism guard: the same seed yields an identical observable trace.
/// If this diverges, the core gained a hidden nondeterministic dependency
/// (wall clock, rand, hash-map iteration leaking into outputs).
#[test]
fn udp_simulation_is_deterministic() {
    fn fingerprint(seed: u64) -> (u64, u64, u64) {
        let steps = 1_500;
        let sink: FingerprintSink = Arc::new(Mutex::new(Vec::new()));
        let report = SimulationBuilder::new()
            .workload(UdpSimWorkload {
                steps,
                verbose: false,
                sink: Some(sink.clone()),
                // Swarm stays ON here: the guard then also covers the config
                // draw itself (a nondeterministic draw would fork the trace).
                swarm: true,
                config_sink: None,
            })
            .set_debug_seeds(vec![seed])
            // Pin the run to exactly this seed (see the replay-path comment).
            .set_iterations(1)
            .run_time_budget(run_budget(steps))
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
    assert_eq!(
        a.0, a.1,
        "final close_all must evict everything created (no leak)"
    );
}

/// Swarm-config stability: the configuration drawn for a seed is a pure
/// function of that seed. Two fresh runs of the same seed must record the
/// identical [`SwarmConfig`] (and therefore print a byte-identical
/// `swarm-config` line — the replay contract).
#[test]
fn udp_swarm_config_is_stable_across_draws() {
    fn draw(seed: u64) -> SwarmConfig {
        let steps = 8;
        let sink: ConfigSink = Arc::new(Mutex::new(Vec::new()));
        let report = SimulationBuilder::new()
            .workload(UdpSimWorkload {
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
        let v = sink.lock().unwrap();
        *v.first().expect("workload recorded a swarm config")
    }

    // Several seeds so both the reserved full configuration and subsets are hit.
    for seed in [0x0Au64, 0x0B, 0x0C, 0x5EED_5EED, 0xDEAD_BEEF] {
        let a = draw(seed);
        let b = draw(seed);
        assert_eq!(
            a, b,
            "seed={seed:#x}: swarm config must be identical across two draws"
        );
        assert!(
            a.is_enabled(Action::ClientDatagram),
            "seed={seed:#x}: the MANDATORY ClientDatagram feature must always be enabled"
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
fn udp_campaign_reserves_one_inclusive_seed_per_four() {
    for count in [1usize, 3, 4, 5, 256] {
        let seeds = campaign_seeds(count);
        let inclusive = seeds.iter().filter(|seed| inclusive_seed(**seed)).count();
        assert_eq!(inclusive, count.div_ceil(4), "count={count}");
        assert_eq!(seeds.len() - inclusive, count - count.div_ceil(4));
    }
}

#[test]
fn env_parse_accepts_hex_and_decimal() {
    assert_eq!(parse_u64("42"), Some(42));
    assert_eq!(parse_u64(" 256 "), Some(256));
    assert_eq!(parse_u64("0xdeadbeef"), Some(0xdead_beef));
    assert_eq!(parse_u64("0XFF"), Some(0xFF));
    assert_eq!(parse_u64("notanumber"), None);
}
