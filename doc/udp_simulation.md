# Deterministic simulation (UDP)

`sim/tests/udp_simulation.rs` (the **`sozu-sim`** crate) is a FoundationDB/VOPR-style
**deterministic simulation test** for the sans-io UDP load-balancing core
(`sozu_lib::protocol::udp::UdpManager` + `UdpFlow`, issue #1273), driven by the
[`moonpool-sim`][moonpool] deterministic-simulation engine for Rust (in the spirit
of [TigerBeetle's VOPR][vopr] and the [FoundationDB simulator][fdb]): a seeded RNG
drives a randomized, adversarial workload against the pure core across many seeds,
so that **same seed → identical run**, and every failure reproduces from its seed.

It replaced an earlier handmade synchronous harness; the action grammar, shadow
model, invariants, and replay knobs below carried over essentially unchanged.

> **moonpool engine, scoped flag.** The harness runs as an `async` moonpool
> `Workload` that draws from moonpool's seeded RNG, advances moonpool's virtual
> clock, and steps the **synchronous** `UdpManager` between awaits — so `lib/`
> stays async-free (moonpool/tokio are `sim/` dev-dependencies only). moonpool
> needs `--cfg tokio_unstable` (it seeds tokio's runtime RNG via `RngSeed` for
> scheduler determinism); that flag is **scoped to the `sozu-sim` build only** —
> the moonpool dev-deps sit under `[target.'cfg(tokio_unstable)'.dev-dependencies]`
> and the test is `#![cfg(tokio_unstable)]`-gated, so a plain `cargo test --workspace`
> builds it as an empty 0-test binary and the flag never touches the rest of the
> workspace. Run it with
> `RUSTFLAGS="--cfg tokio_unstable" cargo test -p sozu-sim --test udp_simulation`.

## Why deterministic simulation fits here

The UDP core is **pure sans-io**: time is an injected `now: Instant`, the hash
seed is injected at construction, and there is no socket, no `Instant::now()` in
the hot path, and no `rand`. That purity is exactly what makes deterministic
replay possible. The core is also packed with `debug_assert` invariants plus a
`check_invariants()` post-condition that runs at the end of every public
mutating method (in debug / test builds — see `manager.rs::check_invariants`).
The simulator's job is to drive the core hard enough, across enough seeds, that
those embedded assertions — and the higher-level model invariants the harness
adds — fire on any regression.

## Virtual clock

moonpool's virtual clock advances only when the workload awaits
`ctx.time().sleep(delta)`; `ctx.time().now()` returns the elapsed `Duration` since
the simulation start. The core takes a `std::time::Instant`, so a base `Instant`
is captured **once** per seed and the simulated `Duration` is added to it. The
core only compares `Instant`s relatively, so the absolute base is irrelevant and a
run is a pure function of its seed.

## Action grammar (weighted)

Each step draws one weighted-random action:

| weight | action            | what it exercises                                  |
|--------|-------------------|----------------------------------------------------|
|   34   | `ClientDatagram`  | admission, reuse, buffer, forward (incl. empty / oversized payloads) |
|   16   | `BackendResolved` | await→established transition (valid + stale id)     |
|   14   | `BackendDatagram` | NAT return path (valid + unknown / closed id)       |
|   14   | `AdvanceClock`    | idle reaper (small jumps + occasional mass-reap)    |
|    6   | `ReconfigCluster` | reconfig storms (affinity / caps / PPv2 / timeouts) |
|    5   | `SetMaxFlows`     | cap shrink **below** live count + grow              |
|    4   | `AbortFlow`       | on-demand teardown of a random id                   |
|    3   | `SetMaxRx`        | max-rx change → truncation boundary                 |
|    2   | `Drain`           | verified drain-to-zero + fresh-listener rebuild     |
|    2   | `CloseAll`        | mass teardown in place                              |

After **every** action the workload fully drains `poll_output()` to `None`,
folding each `Output` into a shadow model (tracking `FlowCreated` / `FlowEvicted`
for active-flow accounting) and honouring a subset of `SelectBackend` requests
with `BackendResolved` so flows progress to `Established`.

`Drain` is special: the core's `draining` flag is a one-way latch (there is no
"resume"), mirroring the shell draining a listener and then dropping it. The
simulator therefore treats `Drain` as a self-contained **listener-lifecycle
episode** — drain, reap, verify the table drains to zero, then stand up a fresh
manager — so the long run keeps admitting afterwards.

## Buggify

`buggify_with_prob!(p)` is moonpool's [FoundationDB `buggify`][fdb-buggify]
primitive: with low per-call probability it injects an *extra* adversarial event
(stale `BackendResolved`, a reconfig burst, a `max_flows` shrink to a tiny value,
a giant clock jump). It only ever runs under simulation.

## Invariants

The core's own `debug_assert` invariants fire for free during each `handle_*`
call (table↔slab consistency, table injectivity, `Closing` never persists,
phase↔backend coherence, cap high-water bound, timer coherence, and the
strict-advance "next deadline > now after a firing" busy-loop guard). The
harness adds, after every fully-drained step:

- `flow_count() <= high-water mark of every cap ever set`.
- after draining, `poll_output()` is `None`.
- `poll_timeout()` is `None` (no live flows) or `Some` (live flows) — coherent
  with `flow_count()`.
- **model balance**: `created_seen - evicted_seen == flow_count()` (active-flow
  accounting; no underflow, no leak).
- no panic for any input (empty / oversized / stale / unknown — "silence is a
  virtue").
- **FINAL**: after a clock jump past every idle deadline + `close_all`, the
  manager drains to zero — `flow_count() == 0`, `poll_timeout() == None`,
  `created == evicted`.

## Running, sweeping, replaying

The per-PR `udp-simulation` CI job runs a modest seeded sweep; the nightly
`simulation-sweep` workflow goes deep. Locally the `--cfg tokio_unstable` flag is
**required** — without it the `sozu-sim` crate compiles to an empty 0-test binary:

```bash
RUSTFLAGS="--cfg tokio_unstable" cargo test -p sozu-sim --test udp_simulation
```

Replay a single CI failure verbosely (seed accepts decimal or `0x`-hex):

```bash
RUSTFLAGS="--cfg tokio_unstable" SOZU_UDP_SIM_SEED=0xdeadbeef \
  cargo test -p sozu-sim --test udp_simulation
```

Widen / deepen the sweep (FoundationDB nightly seed-sweep analog):

```bash
RUSTFLAGS="--cfg tokio_unstable" SOZU_UDP_SIM_SEEDS=1024 SOZU_UDP_SIM_STEPS=5000 \
  cargo test -p sozu-sim --test udp_simulation udp_simulation_seed_sweep -- --nocapture
```

| env var               | effect                                                |
|-----------------------|-------------------------------------------------------|
| `SOZU_UDP_SIM_SEED`   | run that ONE seed with verbose tracing (replay)       |
| `SOZU_UDP_SIM_SEEDS`  | sweep `n` seeds instead of the default                |
| `SOZU_UDP_SIM_STEPS`  | run `n` steps per seed instead of the default         |

A failing seed is surfaced in moonpool's `SimulationReport` (and the panicking
invariant prints the seed + step), so the run reproduces via the
`SOZU_UDP_SIM_SEED` command above. Seed *values* are moonpool's own (not portable
from the former handmade harness).

[vopr]: https://github.com/tigerbeetle/tigerbeetle/blob/main/docs/internals/vopr.md
[fdb]: https://apple.github.io/foundationdb/testing.html
[fdb-buggify]: https://transactional.blog/simulation/buggify
[moonpool]: https://crates.io/crates/moonpool-sim
