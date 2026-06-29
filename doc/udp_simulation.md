# Deterministic simulation (UDP)

`lib/tests/udp_simulation.rs` is a FoundationDB/VOPR-style **deterministic
simulation test** for the sans-io UDP load-balancing core
(`sozu_lib::protocol::udp::UdpManager` + `UdpFlow`, issue #1273). It is built in
the spirit of [TigerBeetle's VOPR][vopr], the [FoundationDB simulator][fdb], and
the [`moonpool-sim`][moonpool] deterministic-simulation engine for Rust:
a single seeded RNG drives a randomized, adversarial workload against the pure
core across many seeds, so that **same seed → bit-for-bit identical run**, and
every failure reproduces exactly from its seed.

> **Two harnesses, one core.** This document describes the **handmade**
> synchronous harness (`lib/tests/udp_simulation.rs`), the pure-core driver and
> the deep CI swarm. A second harness in the **`sozu-sim`** crate
> (`sim/tests/udp_simulation.rs`) drives the *same* `UdpManager` through the real
> [`moonpool-sim`][moonpool] engine (async workload over moonpool's seeded
> runtime / virtual clock / RNG / `buggify`), keeping `lib/` async-free. It
> ports the same action grammar, shadow model, invariants and the
> `SOZU_UDP_SIM_SEED`/`_SEEDS`/`_STEPS` knobs; it requires `--cfg tokio_unstable`
> (moonpool's `RngSeed`), but **scoped to the sim build only** — the moonpool
> dev-deps and the test are `#[cfg(tokio_unstable)]`-gated, so a plain
> `cargo test --workspace` builds it as an empty 0-test binary and the flag never
> touches the rest of the workspace. Seed *values* are not portable between the
> two (different RNG engines). Run it with
> `RUSTFLAGS="--cfg tokio_unstable" cargo test -p sozu-sim --test udp_simulation`.
> The two run in parallel until per-action / per-drop-reason coverage equivalence
> is proven. See `doc/testing.md` for the rationale.

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

A base `Instant` is captured **once**, outside the stepping loop, and advanced
only by RNG-drawn deltas (ms..seconds). No wall-clock read happens inside the
loop, so a run is a pure function of its seed.

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

After **every** action the harness fully drains `poll_output()` to `None`,
folding each `Output` into a shadow model (tracking `FlowCreated` / `FlowEvicted`
for active-flow accounting) and honouring a subset of `SelectBackend` requests
with `BackendResolved` so flows progress to `Established`.

`Drain` is special: the core's `draining` flag is a one-way latch (there is no
"resume"), mirroring the shell draining a listener and then dropping it. The
simulator therefore treats `Drain` as a self-contained **listener-lifecycle
episode** — drain, reap, verify the table drains to zero, then stand up a fresh
manager — so the long run keeps admitting afterwards.

## Buggify

`buggify(rng, p)` is the [FoundationDB `buggify`][fdb-buggify] analog: with low
per-call probability it injects an *extra* adversarial event (stale
`BackendResolved`, a reconfig burst, a `max_flows` shrink to a tiny value, a
giant clock jump). It only ever runs under simulation.

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
- **model balance**: `created_seen − evicted_seen == flow_count()` (active-flow
  accounting; no underflow, no leak).
- no panic for any input (empty / oversized / stale / unknown — "silence is a
  virtue").
- **FINAL**: after a giant clock jump + `close_all`, the manager drains to zero —
  `flow_count() == 0`, `poll_timeout() == None`, `created == evicted`.

## Running, sweeping, replaying

The default sweep runs as part of `cargo test --workspace` (pure core, no I/O —
256 seeds × 3000 steps in ~2.3 s):

```bash
cargo test -p sozu-lib --test udp_simulation
```

Replay a single CI failure verbosely (seed accepts decimal or `0x`-hex):

```bash
SOZU_UDP_SIM_SEED=0xdeadbeef cargo test -p sozu-lib --test udp_simulation
```

Widen / deepen the sweep (FoundationDB nightly seed-sweep analog):

```bash
SOZU_UDP_SIM_SEEDS=1024 SOZU_UDP_SIM_STEPS=5000 \
  cargo test -p sozu-lib --test udp_simulation udp_simulation_seed_sweep -- --nocapture
```

| env var               | effect                                                |
|-----------------------|-------------------------------------------------------|
| `SOZU_UDP_SIM_SEED`   | run that ONE seed with verbose tracing (replay)       |
| `SOZU_UDP_SIM_SEEDS`  | sweep `0..n` seeds instead of `0..256`                |
| `SOZU_UDP_SIM_STEPS`  | run `n` steps per seed instead of the default         |

On any failure the panic message carries the failing **seed + step**, so the run
reproduces exactly via `SOZU_UDP_SIM_SEED=<seed>`.

[vopr]: https://github.com/tigerbeetle/tigerbeetle/blob/main/docs/internals/vopr.md
[fdb]: https://apple.github.io/foundationdb/testing.html
[fdb-buggify]: https://transactional.blog/simulation/buggify
[moonpool]: https://crates.io/crates/moonpool-sim
