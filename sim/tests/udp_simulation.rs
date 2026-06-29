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
//! # Replay / sweep ergonomics
//!
//! - `SOZU_UDP_SIM_SEED=<u64|0xhex>` — replay that ONE seed verbosely.
//! - `SOZU_UDP_SIM_SEEDS=<n>` — sweep `n` iterations (default 256).
//! - `SOZU_UDP_SIM_STEPS=<n>` — steps per seed (default 3000).
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

#[derive(Clone, Copy, Debug)]
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

/// Draw a weighted-random action (weights sum to 100), mirroring the handmade
/// grammar so coverage is preserved.
fn pick_action(ctx: &SimContext) -> Action {
    let roll = ctx.random().random_range(0..100u32);
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

struct UdpSimWorkload {
    steps: usize,
    verbose: bool,
    sink: Option<FingerprintSink>,
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
            let action = pick_action(ctx);
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
            // moonpool's fault-injection primitive.
            if buggify_with_prob!(0.02) {
                match ctx.random().random_range(0..4u8) {
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

    // Single-seed replay mode.
    if let Some(seed) = env_u64("SOZU_UDP_SIM_SEED") {
        eprintln!("== UDP sim single-seed replay: seed={seed:#x} steps={steps} ==");
        let report = SimulationBuilder::new()
            .workload(UdpSimWorkload {
                steps,
                verbose: true,
                sink: None,
            })
            .set_debug_seeds(vec![seed])
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
        })
        .set_iterations(seeds)
        .run_time_budget(run_budget(steps))
        .run();
    assert_no_failures(&report);
}

/// Fast smoke test pinning one representative seed.
#[test]
fn udp_simulation_replays_known_seed() {
    let steps = 4_000;
    let report = SimulationBuilder::new()
        .workload(UdpSimWorkload {
            steps,
            verbose: false,
            sink: None,
        })
        .set_debug_seeds(vec![0x5E_ED_C0_DE])
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
            })
            .set_debug_seeds(vec![seed])
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

#[test]
fn env_parse_accepts_hex_and_decimal() {
    assert_eq!(parse_u64("42"), Some(42));
    assert_eq!(parse_u64(" 256 "), Some(256));
    assert_eq!(parse_u64("0xdeadbeef"), Some(0xdead_beef));
    assert_eq!(parse_u64("0XFF"), Some(0xFF));
    assert_eq!(parse_u64("notanumber"), None);
}
