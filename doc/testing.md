# Testing Sōzu

This is the authoritative guide to how Sōzu is tested and the doctrine every
change is held to. It consolidates the rules that were previously scattered
across `CLAUDE.md`, the e2e helpers, and the per-feature docs into a single
reference.

Sōzu is a hot-reconfigurable reverse proxy on the network's critical path: a
silent correctness bug is a dropped connection, a truncated response, or a
security hole. The testing strategy is built to surface those defects *loudly
and early*, in seconds of simulation rather than days of production traffic.

Three public bodies of work shape the doctrine and are cited throughout:

- **TigerBeetle's TigerStyle** — assertion-first programming, assertion density,
  and pair (positive + negative space) assertions.
  <https://github.com/tigerbeetle/tigerbeetle/blob/main/docs/TIGER_STYLE.md>
- **Apple FoundationDB's deterministic simulator** — single-threaded seeded
  simulation, `buggify` fault injection, nightly seed swarms, and exact replay
  of a failing seed. <https://apple.github.io/foundationdb/testing.html>
- **`moonpool-sim`** — a deterministic-simulation engine that brings the
  FoundationDB approach (seeded RNG, virtual clock, in-process network) to Rust.
  Sōzu uses it for the UDP core simulation, hosted in the **`sim/` (`sozu-sim`)**
  crate (`sim/tests/udp_simulation.rs`): the workload runs as an `async` moonpool
  task that draws from moonpool's RNG, advances its virtual clock, and steps the
  **synchronous** sans-io `UdpManager` between awaits, so `lib/` stays async-free
  (moonpool/tokio are `sim/` dev-dependencies only). The engine requires
  `--cfg tokio_unstable` (it seeds tokio's runtime RNG via `RngSeed` for scheduler
  determinism), but **scoped to the sim build only**: the moonpool dev-deps sit
  under `[target.'cfg(tokio_unstable)'.dev-dependencies]` and the test is
  `#![cfg(tokio_unstable)]`-gated, so a plain `cargo test --workspace` builds
  `sozu-sim` as an empty 0-test binary and the flag never touches the rest of the
  workspace; run it with `RUSTFLAGS="--cfg tokio_unstable" cargo test -p sozu-sim`.
  This **replaced** the earlier handmade direct-seeded loop (formerly
  `lib/tests/udp_simulation.rs`); the in-source `check_invariants()` sweep
  (`manager.rs`/`flow.rs`) and the `fuzz_udp_flow` target remain the
  harness-independent safety net and are exercised on every debug build.
  <https://crates.io/crates/moonpool-sim>

---

## 1. Philosophy

**Assertions downgrade silent correctness bugs into loud crashes.** A wrong
value that would otherwise propagate quietly — a dangling table key, a gauge
that underflows, a flow that outlives its cap — trips a `debug_assert!` at the
exact instruction that produced it. The failure carries a stack trace and (under
simulation) a reproducing seed, instead of surfacing hours later as a confusing
symptom three layers away. This is the central bet of TigerStyle, and Sōzu takes
it: every non-trivial state transition states what it expects *and* what it
forbids.

**Sans-io cores are deterministically simulatable.** A "sans-io" core is a pure
state machine with no socket, no `Instant::now()` on the datapath, and no `rand`
— time is an injected `now: Instant` and any hashing seed is injected at
construction. The UDP load-balancing core
(`lib/src/protocol/udp/{manager,flow}.rs`) is built exactly this way. Purity is
what makes *bit-for-bit deterministic replay* possible: the same seed always
produces the same run, so any failure reproduces exactly.

**Random workload + fault injection + dense assertions finds bugs in seconds.**
A seeded RNG driving a randomized, adversarial workload against a pure core,
with FoundationDB-style `buggify` fault injection and a full-sweep
`check_invariants()` after every step, explores state space that real traffic
would take days to reach. The bug that needs a precise interleaving of a
reconfig storm, a cap shrink below the live count, and a mass-reap clock jump is
found on some seed in the sweep, not in an incident report.

**Panics are for invariant violations, never for adversarial input.** Sōzu's
no-panic rule (`CLAUDE.md > Code style`) is load-bearing here: on the *release*
datapath, invalid or hostile traffic is turned into a `SessionResult`, an H2
`GOAWAY`/`RST_STREAM`, or a default HTTP answer plus a metric and a contextual
log — never a panic. `assert!`/`panic!`/`unreachable!` are reserved for genuine
internal invariants and for tests. `debug_assert!` is the workhorse: it is
compiled out in release (so adversarial input can never trip it in production)
but is live in every test, e2e, fuzz, and developer build, where it does the
catching.

---

## 2. Test taxonomy

| Category | Where it lives | Needs | Gating | Per-PR? |
|---|---|---|---|---|
| **Unit** | `#[cfg(test)] mod tests` beside each module in `lib/src/**`, `command/src/**` | nothing beyond `protoc` + toolchain | run by `cargo test -p sozu-lib` | yes |
| **Integration / e2e** | `e2e/src/tests/*` (registered in `e2e/src/tests/mod.rs`), mocks in `e2e/src/mock/*` | spawns real workers + mock clients/backends; `h2spec` for one conformance test | `cargo test -p sozu-e2e` | yes |
| **Fuzz** | `fuzz/fuzz_targets/*` (out-of-workspace `sozu-fuzz` crate) | nightly toolchain + `cargo-fuzz` | `fuzz` CI job (nightly toolchain, 300 s/target on every push/PR); `#[ignore]`-style runtime skip when prereqs absent | yes (300 s/target); daily 900 s sweep in `simulation-sweep.yml` |
| **Deterministic simulation** | `sim/tests/udp_simulation.rs` (`sozu-sim`, moonpool-sim) | `RUSTFLAGS="--cfg tokio_unstable"` (scoped to the sim — cfg-gated, off by default) | per-PR `udp-simulation` job (modest sweep) + nightly deep swarm; widened via env knobs | yes |
| **Deterministic simulation** | `sim/tests/tcp_preread_sim.rs` (`sozu-sim`, moonpool-sim) — TCP SNI-preread core, [#1279](https://github.com/sozu-proxy/sozu/issues/1279) | same `--cfg tokio_unstable` gating as above | per-PR `udp-simulation` job (same job, added step, modest sweep) + nightly `tcp-preread-simulation-sweep` job in `simulation-sweep.yml` (deep swarm); widened via `SOZU_TCP_PREREAD_SIM_*` env knobs | yes |
| **Deterministic simulation** | `sim/tests/metrics_lease_sim.rs` (`sozu-sim`, moonpool-sim) — metrics cardinality-lease core (`Aggregator::lease_apply`/`lease_clear`/`lease_tick` plus the `remove_cluster`/`add_cluster`/`remove_backend` tombstone) | same `--cfg tokio_unstable` gating as above | no CI job yet — run manually with the command below; widened via `SOZU_METRICS_LEASE_SIM_*` env knobs | no (not yet wired into CI — see this section's closing note) |
| **Regression guards** | `lib/tests/log_layout.rs` | nothing | runs in `cargo test -p sozu-lib`; build-time `cargo:warning=` echo from `lib/build.rs` | yes |

Notes:

- The e2e suite includes H1/H2/TLS/TCP/UDP coverage plus targeted security and
  feature suites; see `e2e/src/tests/mod.rs` for the full module list
  (`h2_tests`, `h2_security_*`, `mux_tests`, `tls_tests`, `tcp_tests`,
  `tcp_sni_tests` (SNI+ALPN passthrough routing, #1279), `udp_tests`,
  `command_channel_security_tests`, `listener_update_tests`, …).
- The TCP SNI-preread sans-io core (`lib/src/protocol/tcp_preread/{mod,shell}.rs`)
  follows the same unit-test + assertion-density pattern as the UDP core (§4):
  a `decided`/`deadline` latch, a `check_invariants()` sweep run at both ends of
  `handle_input`, and one unit test per reachable `RejectReason` variant. See
  `lib/src/protocol/tcp_preread/LIFECYCLE.md` for the full lifecycle.
- `sim/tests/metrics_lease_sim.rs` has no CI job. A `metrics-lease-simulation`
  job analogous to `udp-simulation` (a modest per-PR sweep) plus a
  `metrics-lease-simulation-sweep` job in `simulation-sweep.yml` (deep nightly
  swarm) is proposed but intentionally not added by the change that introduced
  this simulator — wiring CI is a separate decision. Run it manually with the
  command in "Targeted runs" until that decision is made.
- **Router hostname resolution is unit-tested with `quickcheck`**
  (`lib/src/router/mod.rs`,
  `qc_router_hostname_resolution_matches_the_documented_semantics`), on top of
  the example-based regression tests pinning individual fixed bugs. The
  property is checked against an ORACLE independent of `pattern_trie` —
  written fresh from `doc/configure.md`'s "Hostname precedence" and "Regex
  hostname segments" sections rather than by calling the trie's own matching
  functions — over a generator biased toward mixed exact/wildcard/regex rules,
  case variation, uppercase regex escapes, alternations, permuted declaration
  order, and a small pool of distinct declared PATHS crossed against every
  declared hostname: the shapes behind sozu#1349, #1351, #1356 and #1377. Path
  varies deliberately — sozu#1351's real symptom (an exact rule leaking onto a
  regex family) is only observable as a routing MISMATCH when two colliding
  fronts declare different paths; pinned to one shared path, the same leak
  still happens but surfaces as a refused insert instead. Two known limits,
  noted in the harness's own doc comment rather than left implicit: it has no
  reproducible seed (`quickcheck` 1.1.0 seeds `Gen` from OS entropy, unlike the
  FoundationDB-style simulators in §5 below), so a CI failure is reproduced by
  re-running with a raised `QUICKCHECK_TESTS`, not by seed replay; and its
  regex anchoring convention (`\A(?:…)\z`, non-capturing) matches
  `doc/configure.md`'s own prose because that prose was itself added by the
  sozu#1356 fix, so that half is a shared convention rather than an
  independent spec — the case-folding half is genuinely RFC 9110 §4.2.3
  derived. `lib/src/router/pattern_trie.rs`'s `qc_insert` is the same
  technique applied to raw trie insert/lookup, independent of routing
  semantics.
- **H2 HEADERS+CONTINUATION reassembly is unit-tested with `quickcheck`**
  (`lib/src/protocol/mux/h2.rs`,
  `reassembly_property::qc_h2_header_reassembly_survives_interleaved_control_frame_flushes`),
  on top of three example-based pinning tests for the individual fixed bugs
  (`a_legitimate_continuation_survives_an_unrelated_window_update_flush` #1397,
  `a_legitimate_continuation_survives_a_graceful_goaway` #1401,
  `a_legitimate_continuation_survives_a_hup_while_draining` #1423). This one
  is a `ConnectionH2` state-machine property, not a pure-function one like the
  router's: it drives a real connection (`test_h2_connection`, a loopback
  socket, a `Pool`, a `Router`) through `readable()`/`writable()`, splitting a
  real HPACK field block into 2..=5 CONTINUATION fragments and, before each
  one after the first, independently choosing one of four interleaves —
  nothing, an unrelated WINDOW_UPDATE flush, a HUP-while-draining event, or a
  deferred `graceful_goaway` — composing all three deterministic triggers
  above (not just two of them, an earlier version of this property claimed to
  but did not) in every order and count `quickcheck` cares to generate. Each
  interleave point also independently chooses to fire at a frame boundary or
  mid-frame — splitting that CONTINUATION fragment's own bytes across two
  separate simulated TCP segments — so the accumulator holds genuinely
  unretired bytes when the interleave lands, not just an empty `zero.storage`
  between whole-frame writes. Deliberately kept OUT of the `fuzz_frame_parser` target
  (§6), which is scoped to the stateless `protocol::mux::parser` functions:
  `ConnectionH2` is a private, stateful type needing a real socket/`Pool`/
  `Router` that the out-of-workspace fuzz crate cannot reach without exposing
  internals it has no other reason to expose, whereas `#[cfg(test)] mod tests`
  inside `h2.rs` already has all of it. CONTINUATION-flood abort and the
  CVE-2024-27316 refusal path are intentionally not folded into this
  property — see its own doc comment for why — and stay covered by
  `a_refused_stream_keeps_the_hpack_decoder_in_sync` and
  `a_refused_padded_prioritized_stream_keeps_the_hpack_decoder_in_sync`.
- **The H2 write pass's HPACK encoder is unit-tested with `quickcheck`**
  (`lib/src/protocol/mux/h2.rs`,
  `write_pass_property::qc_one_write_pass_prefixes_its_first_header_block_only`),
  on top of the example-based
  `one_write_pass_prefixes_its_first_header_block_only_and_shares_one_encoder`.
  Same `ConnectionH2` state-machine technique as the reassembly property above
  and a deliberate sibling of it rather than a case inside it: that one drives
  the READ path and its oracle is "one decoder stays in sync across a
  fragmented header block", this one drives the WRITE path and its oracle is
  "one encoder, one RFC 7541 §6.3 size-update prefix, N blocks a single peer
  decoder replays in order". One `quickcheck` verdict over two unrelated state
  machines would say nothing about either. It opens 2..=5 streams with real
  HEADERS frames, gives each sozu's own 404 default answer through
  `answers::set_default_answer`, runs ONE `writable()` pass, and checks the
  captured wire bytes. Both properties are invisible to `converter.rs`'s own
  unit tests, which drive a single `H2BlockConverter` over a single kawa, and
  both became cross-stream rather than structural when the converter was
  scoped to a single `kawa.prepare` call — see LIFECYCLE.md invariant 25. The
  example-based test carries the negative half the property deliberately
  omits: a peer decoder that never saw the first block must FAIL on the
  second, which a generated table size small enough to evict the dynamic table
  would make untrue.
- **RFC 9218 §4 incremental scheduler fairness is unit-tested with `quickcheck`**
  (`lib/src/protocol/mux/h2_scheduler.rs`,
  `fairness_property::qc_incremental_leadership_visits_every_peer_once_per_cycle`),
  on top of the example-based
  `incremental_leadership_rotates_one_position_per_pass` and
  `no_incremental_peer_is_starved_over_a_full_cycle`. A third sibling of the
  two properties above rather than a case inside either: those drive
  `ConnectionH2`'s HPACK decoder across a fragmented read and its encoder
  across one write pass, while this one drives `H2Scheduler`'s round-robin
  cursor across MANY passes, and its oracle is the cyclic successor in the
  generated plan's own ascending stream-id list — computed from the plan, never
  from anything the scheduler returned. One `quickcheck` verdict over three
  unrelated state machines would say nothing about any of them. It generates
  2..=8 same-urgency incremental peers with arbitrary ids and gaps, an
  arbitrary urgency bucket, 1..=4 full cycles, and a distractor set
  (non-incremental peers in the same bucket, incremental streams in a
  lower-priority bucket) that must not perturb the rotation, then asserts both
  the exact leader sequence and the starvation bound: every peer leads exactly
  once per cycle. Of the deterministic pair, one fixes four peers over eight
  passes and the other deliberately goes to five rather than stopping at two,
  because a rotation bug that swaps a pair still looks fair on two streams and
  starves the fifth. All of it is scoped to the urgency bucket that supplies
  the pass leader: the round-robin cursor is one connection-global stream id,
  so a trailing bucket can be permanently static, which
  `the_round_robin_cursor_is_connection_global_so_only_the_leading_bucket_rotates`
  pins as observed behaviour. See LIFECYCLE.md invariant 26. Each test carries
  a `TO SEE THIS RED` recipe naming the statement to delete and the panic it
  produces — except that last one, which asserts what the code already does
  and says so.
- `e2e/src/tests/fuzz_tests.rs` is a thin integration wrapper that shells out to
  the four fuzz targets for 10 s each. It *skips gracefully* (prints a notice,
  returns clean) when the nightly toolchain or `cargo-fuzz` is missing, so the
  rest of the e2e suite still runs. CI skips it in the per-cell pipeline (`--skip
  tests::fuzz_tests::`) and runs real fuzzing in the dedicated `fuzz` job
  (nightly toolchain, 300 s per target on every push/PR), which has a step for
  each of the four targets, including `fuzz_tcp_clienthello` (see §6). A fifth
  target, `fuzz_command_channel` (the command-channel IPC framing, see §6), is
  not yet wired into either the wrapper or the CI job -- that wiring is
  proposed, not added, alongside this target's introduction.

---

## 3. Running the suites

All commands assume `protoc` is installed and the `1.93.1` toolchain pinned by
`rust-toolchain` (CI exercises stable/beta/nightly on top of that).

### Full local validation chain

Run this before pushing — it mirrors what CI gates on:

```bash
cargo build --all-features --locked
cargo +nightly fmt --all -- --check        # nightly REQUIRED (rustfmt.toml uses `ignore = [...]`)
cargo doc --no-deps --all-features --locked
cargo clippy --all-targets --locked -- -D warnings
cargo test --workspace --locked            # unit + simulation + regression guards + e2e
```

### Targeted runs

```bash
# Unit + lib-level tests (includes the log-layout guard; the UDP,
# TCP-preread, and metrics-lease deterministic simulations moved to the
# sozu-sim crate — see the targeted `-p sozu-sim` invocations below):
cargo test -p sozu-lib --locked

# A single test module / filter:
cargo test -p sozu-e2e -- h2_              # all H2 e2e tests
cargo test -p sozu-e2e -- test_udp_        # all UDP e2e tests
cargo test -p sozu-e2e test_upgrade        # worker-upgrade e2e (see doc/upgrade_e2e_tests.md)

# Deterministic UDP simulation (moonpool-sim, sozu-sim crate): cfg-gated, so the
# flag is REQUIRED — without it the crate compiles to an empty 0-test binary:
RUSTFLAGS="--cfg tokio_unstable" cargo test -p sozu-sim --test udp_simulation

# Deterministic TCP SNI-preread simulation (same crate, same cfg gating):
RUSTFLAGS="--cfg tokio_unstable" cargo test -p sozu-sim --test tcp_preread_sim

# Deterministic metrics cardinality-lease simulation (same crate, same cfg gating):
RUSTFLAGS="--cfg tokio_unstable" cargo test -p sozu-sim --test metrics_lease_sim
```

### Simulation sweep + single-seed replay

The UDP simulator reads three env knobs (`sim/tests/udp_simulation.rs`,
`doc/udp_simulation.md`):

| env var | effect |
|---|---|
| `SOZU_UDP_SIM_SEED` | run that ONE seed verbosely (decimal or `0x`-hex) — the replay path |
| `SOZU_UDP_SIM_SEEDS` | sweep `0..n` seeds instead of the default `0..256` |
| `SOZU_UDP_SIM_STEPS` | run `n` steps per seed instead of the default `3000` |
| `SOZU_SIM_SWARM` | `0` pins every seed to the inclusive all-features configuration; default `1` draws a per-seed swarm subset. Shared by BOTH simulators — see §5 "Swarm configurations" |

```bash
# Reproduce a CI failure exactly (the panic prints the failing seed + step):
RUSTFLAGS="--cfg tokio_unstable" SOZU_UDP_SIM_SEED=0xdeadbeef \
  cargo test -p sozu-sim --test udp_simulation

# Widen / deepen the sweep (FoundationDB nightly seed-swarm analog):
RUSTFLAGS="--cfg tokio_unstable" SOZU_UDP_SIM_SEEDS=1024 SOZU_UDP_SIM_STEPS=5000 \
  cargo test -p sozu-sim --test udp_simulation udp_simulation_seed_sweep -- --nocapture
```

The TCP SNI-preread simulator (`sim/tests/tcp_preread_sim.rs`) mirrors the
same replay/sweep contract under its own env-var namespace
(`SOZU_TCP_PREREAD_SIM_SEED` / `_SEEDS` / `_STEPS`, default 256 seeds × 48
connections per seed):

```bash
RUSTFLAGS="--cfg tokio_unstable" SOZU_TCP_PREREAD_SIM_SEED=0xdeadbeef \
  cargo test -p sozu-sim --test tcp_preread_sim
```

The metrics cardinality-lease simulator (`sim/tests/metrics_lease_sim.rs`,
`sozu_lib::metrics::Aggregator`'s `lease_apply`/`lease_clear`/`lease_tick`
plus the `remove_cluster`/`add_cluster`/`remove_backend` tombstone) mirrors
the same contract under `SOZU_METRICS_LEASE_SIM_SEED` / `_SEEDS` / `_STEPS`
(default 256 seeds × 1200 steps):

```bash
RUSTFLAGS="--cfg tokio_unstable" SOZU_METRICS_LEASE_SIM_SEED=0xdeadbeef \
  cargo test -p sozu-sim --test metrics_lease_sim
```

### Fuzzing

```bash
# From inside fuzz/ (requires cargo-fuzz + nightly):
cargo +nightly fuzz run fuzz_frame_parser
cargo +nightly fuzz run fuzz_hpack_decoder
cargo +nightly fuzz run fuzz_udp_flow
cargo +nightly fuzz run fuzz_tcp_clienthello
cargo +nightly fuzz run fuzz_command_channel

# Bounded run (what the e2e wrapper and CI do):
cargo +nightly fuzz run fuzz_frame_parser -- -max_total_time=300
```

---

## 4. Assertion density (TigerStyle)

The standard, applied to every non-trivial function on a sans-io core, a parser,
a state machine, the mux, or the command channel:

- **≥ 2 meaningful assertions per non-trivial function** — enough to pin down the
  function's contract, not box-ticking.
- **Pre- and post-conditions.** Assert the inputs you require on entry and the
  state you guarantee on exit.
- **Pair assertions (positive + negative space).** Assert what you expect *and*
  what you forbid. A teardown reason is returned **iff** a cap is truly
  exhausted; a flow within both caps must **not** report a cap-driven teardown.
  See `UdpFlow::teardown_reason` (`lib/src/protocol/udp/flow.rs`), which asserts
  both directions of the boundary.
- **A `check_invariants()` full-sweep post-condition** at the end of every public
  state-machine entry point. `UdpManager` runs `check_invariants()` (via the
  `debug_assert_invariants()` wrapper) at the tail of `handle_input` and
  `handle_timeout` (`lib/src/protocol/udp/manager.rs`), so the *entire* data
  structure is revalidated after every mutation, not just the field that changed.
- **`debug_assert!`, not `assert!`, on the datapath.** `debug_assert!` is
  compiled out in release, so it lives in every test/e2e/fuzz/dev build and
  vanishes in production. The release assignment is always unconditional so
  release behaviour is identical to debug (see `UdpFlow::set_phase`, where the
  legality check is `#[cfg(debug_assertions)]` but the assignment is not).
- **Never `assert!`/panic on network-controlled input on the release path.**
  Hostile bytes become a `SessionResult` / `GOAWAY` / default answer + metric +
  log. This is the no-panic rule from `CLAUDE.md > Code style`.

### Where assertion density is *required*

Sans-io cores, parsers, the H2/H1 mux, every state machine, and the command
channel. These are the surfaces where a wrong intermediate value is both easy to
introduce and expensive to debug from a symptom.

### Worked example: the UDP core

`lib/src/protocol/udp/{manager,flow}.rs` is the reference. `flow.rs` carries 12
`debug_assert`s and `manager.rs` carries 45, including the full
`check_invariants()` sweep. The invariants `UdpManager::check_invariants`
enforces (`lib/src/protocol/udp/manager.rs`):

1. **Table → slab consistency** — every `FlowId` in the routing table points at a
   live slab slot (no dangling keys).
2. **Table injectivity** — no two flow keys map to the same `FlowId`.
3. **`flow_count()` == slab population** — the public count never drifts from the
   real population.
4. **No `Closing` flow persists** in the slab (`close_flow` sets `Closing` and
   removes the slot in the same call). Pair: every live flow is `AwaitingBackend`
   or `Established`.
5. **Phase ↔ backend coherence** — `Established` ⇔ `backend_addr.is_some()`;
   `AwaitingBackend` ⇔ `backend_addr.is_none()` (asserted both directions).
6. **Timer coherence** — `armed_deadline.is_some()` ⇔ at least one live flow
   exists, and when set equals the minimum idle deadline over live flows.
7. **Cap / counter coherence** — a flow that exhausted a cap reports a teardown
   reason; a flow within both caps reports none. Plus the high-water bound:
   `flows.len() <= max_flows_high_water` at all times (a `SetMaxFlows` shrink
   sheds *future* flows but does not evict live ones).

Per-method, `flow.rs` adds the monotonic-counter guards (`requests_seen` /
`responses_seen` saturate and never regress), the legal-transition guard in
`set_phase` (strictly forward `AwaitingBackend → Established → Closing`, with the
only skip being an abort into `Closing`), and the generation-token guard in
`touch` (a touch *must* advance `timer_gen`, defeating the stale-close
busy-loop).

---

## 5. Deterministic simulation

`sim/tests/udp_simulation.rs` (the `sozu-sim` crate) is a VOPR/FoundationDB-style
deterministic simulation of the sans-io UDP core, driven by the
[`moonpool-sim`](https://crates.io/crates/moonpool-sim) engine. The full design is
in `doc/udp_simulation.md`; the essentials:

**How it works.** The harness is a moonpool `Workload` run across many seeded
iterations. moonpool supplies the seeded RNG (`ctx.random()`), the virtual clock
(`ctx.time()`), and the `buggify` fault-injection vocabulary; the workload draws
one weighted-random action from an adversarial grammar (client/backend datagrams,
backend resolutions, clock advances, reconfig storms, cap shrinks below the live
count, aborts, drains, mass teardown) and steps the **synchronous** sans-io
`UdpManager` between awaits. The core takes a `std::time::Instant`, so a base
`Instant` is captured once per seed and moonpool's elapsed `Duration` is added to
it (the core only compares Instants relatively, so the run stays a pure function
of its seed). After **every** action the workload fully drains `poll_output()` to
`None`, folding outputs into a shadow model that tracks active-flow accounting.

**Invariants checked.** Two layers fire on every step:

- The core's own `debug_assert` invariants (the seven in §4) fire for free inside
  each `handle_*` call.
- The harness adds model-level invariants after each fully-drained step:
  `flow_count()` never exceeds the high-water mark of any cap ever set;
  `poll_output()` is `None` after draining; `poll_timeout()` is coherent with
  `flow_count()`; **model balance** — `created_seen − evicted_seen ==
  flow_count()` (no underflow, no leak); and no panic for any input. A **final**
  clock jump past every idle deadline + `close_all` must drain the manager to zero
  (`flow_count() == 0`, `poll_timeout() == None`, `created == evicted`).

**Buggify.** `buggify_with_prob!(p)` is moonpool's FoundationDB-`buggify`
primitive: with low per-call probability it injects an *extra* adversarial event
(a stale `BackendResolved`, a reconfig burst, a `max_flows` shrink to a tiny
value, a giant clock jump). It runs only under simulation, never in production.

**Replay ergonomics.** A failing seed is surfaced in moonpool's `SimulationReport`
(and the panicking invariant prints the seed + step). Reproduce a run verbosely
with `RUSTFLAGS="--cfg tokio_unstable" SOZU_UDP_SIM_SEED=<seed> cargo test -p
sozu-sim`; widen with `SOZU_UDP_SIM_SEEDS` / `SOZU_UDP_SIM_STEPS`. (Seed *values*
are moonpool's own — not portable from the former handmade harness.) A separate
`udp_simulation_is_deterministic` test asserts the same seed yields a bit-identical
tally, guarding against a hidden nondeterministic dependency leaking into outputs.
The engine needs `--cfg tokio_unstable` (tokio `RngSeed` scheduler determinism),
so the test is `#![cfg(tokio_unstable)]`-gated and its deps sit under
`[target.'cfg(tokio_unstable)'.dev-dependencies]` — the flag is scoped to the
`sozu-sim` build, and `cargo test --workspace` builds it as an empty 0-test binary.

### Swarm configurations (Groce et al., ISSTA 2012)

Both simulators run **swarm testing**
([Groce et al., ISSTA 2012](https://users.cs.utah.edu/~regehr/papers/swarm12.pdf)):
instead of every seed exercising the single all-features workload grammar, each
seed draws a random **configuration** — a subset of the generator features —
from its own seeded RNG *before the first operation*, so the configuration is a
pure function of the seed. Feature omission pays twice: omitting a *suppressor*
lets the workload reach states the full grammar repairs too eagerly, and any
omission concentrates the seed's step budget on the surviving features (the
paper's active-suppression and passive-competition mechanisms).

**What a feature is here.**

- `udp_simulation.rs`: an entry of the weighted `Action` grammar
  (`ACTION_TABLE`). Each buggify arm is tied to its sibling feature (a
  stale-resolution fault makes no sense in a configuration without
  `BackendResolved`) and is skipped — never redrawn — when that feature is off.
- `tcp_preread_sim.rs`: a scenario generator (`GENERATOR_TABLE`), plus the one
  genuinely orthogonal **fragmentation** axis (off → every delivery is
  one-shot, and the forced drip generator leaves the pool). The other
  ClientHello axes (SNI shape, ALPN, GREASE, ECH, multi-record, proxy prefix)
  are embodied by their carrier generators — toggling one inside a directed
  generator would invalidate its expected terminal. The 2% buggify byte-flip
  stays always-on: it is a wire-level fault, not a grammar feature.

**Classification.** A feature is MANDATORY (bootstrap/observation the workload
cannot run without — always retained), OPTIONAL, or a SUPPRESSOR (it repairs
the very state another bug needs — the paper's `pop` call to a stack-overflow
bug):

| Simulator | MANDATORY | SUPPRESSOR | OPTIONAL |
|---|---|---|---|
| UDP | `ClientDatagram` — the sole flow creator; without it every shadow-model invariant is vacuously green | `Drain`, `CloseAll`, `AbortFlow`, `SetMaxFlows` — each evicts flows, resets the manager, or sheds future admissions, repairing the very full-table state a capacity bug needs | `BackendResolved`, `BackendDatagram`, `AdvanceClock`, `ReconfigCluster`, `SetMaxRx` |
| TCP preread | none — `SniPrereadCore` is fresh per connection, and the harness machinery (replay checks, coverage tally) is observation, not a feature | none — no state survives a connection, so nothing can suppress across connections; the swarm benefit here is pure passive competition | all 25 generators, plus the fragmentation axis |
| Metrics lease | `LeaseApply` — the sole lease creator; `RemoveCluster` — the sole tombstone armer; `EmitClusterMetric` — the sole resurrection prober; omitting any of the three makes an entire assertion class vacuously green | `LeaseClear`, `LeaseTick`, `AddCluster` — each shrinks the lease table or clears a tombstone, repairing the very full-table / still-tombstoned state a capacity or resurrection bug needs | `EmitBackendMetric`, `RemoveBackend`, `SetDetail`, `TableCapacityStress`, `BoundaryExpiry`, `RenewalAuthGate` |

**Per-seed draw and campaign composition.** Campaigns run the explicit seed
range `0..n`, and seeds divisible by four keep the inclusive all-features
configuration `C_D` (the paper is explicit that swarm
subsets complement, never replace, it: a bug needing `k` specific features
together appears in a coin-toss subset with probability `1/2^k`). The other
seeds include each optional feature with 50% probability, renormalize the
remaining weights over the survivors, and repair an all-off or all-on draw to
a non-empty proper subset. Every sweep — the per-PR job (64 UDP / 512 TCP
seeds) and the nightly deep swarm — therefore reserves exactly one inclusive
run in each four-seed cohort; shorter final cohorts start with their inclusive
run.

**Replay contract.** The drawn configuration is printed as one canonical
`swarm-config sim=... seed=... mode=... features=[...] total_weight=...` line
before the workload runs, byte-identical across replays of the same seed (the
`*_swarm_config_is_stable_across_draws` tests assert the stability). A failing
seed replayed with `SOZU_UDP_SIM_SEED` / `SOZU_TCP_PREREAD_SIM_SEED` /
`SOZU_METRICS_LEASE_SIM_SEED` therefore always shows its configuration.
`SOZU_SIM_SWARM=0` (shared by all three simulators) disables the draw
entirely — zero extra RNG consumption, byte-identical to the pre-swarm
grammar — so a swarm campaign and an all-features campaign of identical seed
count can be compared directly. The pinned `*_replays_known_seed` smoke tests
run with swarm off for the same reason.

**Coverage gate placement.** The TCP per-class gate
(`CoverageTally::assert_full_coverage`) is asserted on the MERGED campaign
tally, never per seed: a single swarm seed legitimately cannot reach every
`RejectReason` class, but the sweep still must, and still fails loudly when it
does not. If a swarm subset trips an invariant, that is the point of the
exercise: report the failing seed and its printed configuration — never weaken
the assertion or the gate. The metrics-lease simulator's `CoverageTally` (its
thirteen `LeaseApplyOutcome`/`LeaseClearOutcome`/tick/tombstone classes,
including `TableFull`, `TtlOutOfRange`, and the exact-boundary expiry) follows
the same merged-tally placement.

### Recipe: adding a simulator over another sans-io core

The pattern generalizes to any pure state machine. To add one:

1. **Make the core sans-io.** Inject the clock (`now: Instant`) and any hash seed
   at construction; remove `Instant::now()` and `rand` from the datapath.
2. **Pack it with assertions** plus a `check_invariants()` full sweep at the end
   of every public mutating entry point (§4).
3. **Write a moonpool `Workload`** in the `sozu-sim` crate (`sim/tests/`): take the
   clock + RNG from `SimContext` (`ctx.time()` / `ctx.random()`), step the
   synchronous core between awaits, bias a weighted action grammar toward the
   adversarial transitions (reconfig, cap changes, teardown), and fold observable
   outputs into a shadow model. Drive seeds via `SimulationBuilder::set_iterations`
   / `set_debug_seeds`.
4. **Add `buggify_with_prob!`** for low-probability extra adversarial events.
5. **Wire the `SOZU_*_SIM_SEED / _SEEDS / _STEPS` replay knobs**, gate the test on
   `#![cfg(tokio_unstable)]`, and put the moonpool dev-deps under
   `[target.'cfg(tokio_unstable)'.dev-dependencies]` so the flag stays scoped to
   the sim crate.

`sim/tests/metrics_lease_sim.rs` (the metrics cardinality-lease core,
`Aggregator::lease_apply`/`lease_clear`/`lease_tick` plus the cluster/backend
removal tombstone) is a worked example of this recipe end to end: step 1
required parameterising `lease_apply`'s clock (it previously read
`Instant::now()` directly, unlike its sibling `lease_tick`) before the
simulator could exist at all. Its module doc comment names four traps this
project has already paid for and expects not to repeat — a leaked host-clock
read reading as vacuously deterministic under virtual time, a pinned seed
that silently keeps drawing fresh ones, a shadow model that never inspects
the core's actual output, and chaos that is on by default but rarely fires —
read it before adding a fifth simulator.

The H2 mux (`lib/src/protocol/mux/`) is the obvious next candidate — its stream
slot lifecycle, flow-control accounting, and GOAWAY/RST_STREAM semantics are a
state machine of the same shape. This is a stated direction, not a commitment.

---

## 6. Fuzzing

The out-of-workspace `sozu-fuzz` crate (`fuzz/`) has five cargo-fuzz targets
(`fuzz/fuzz_targets/`), each defending a network-facing parser or state machine:

| Target | Surface | Defends against |
|---|---|---|
| `fuzz_frame_parser` | H2 frame parser (`protocol::mux::parser`, RFC 9113 §6) | length-confusion / framing CVEs (`ensure_frame_size!`); parser must reject via `H2Error`/`nom::Err`, never panic |
| `fuzz_hpack_decoder` | HPACK decoder (RFC 7541, `loona-hpack`) under three dynamic-table profiles | header-block oversize and incomplete-update flaws; resize/eviction paths |
| `fuzz_udp_flow` | the sans-io UDP core + flow-key extraction + PPv2 DGRAM framing | flow-count overrun, gauge underflow, fd/slab leak; reuses the same invariants the simulator asserts |
| `fuzz_tcp_clienthello` | the sans-io TCP SNI-preread core (`protocol::tcp_preread`, [#1279](https://github.com/sozu-proxy/sozu/issues/1279)) — TLS record/ClientHello parsing, PROXY-v2 stripping, SNI/ALPN routing | the core mutating the fed byte window, a latched terminal verdict (`Routed`/`Reject`) changing on a later call, `NeedMore` reappearing after a terminal, `content_offset` exceeding the fed window, a `NeedMore` deadline regressing across calls |
| `fuzz_command_channel` | the command channel's length-delimited IPC framing (`sozu_command_lib::channel::Channel`, `command/src/channel.rs`) — `try_read_delimited_message` (reached via the public, purely in-memory `read_message()`) and `write_delimited_message`, against truncated frames, an under-delimiter/zero/oversize/split-across-boundary declared length, and valid frames followed by garbage | a `read_message()` call increasing pending data; a successful decode not consuming exactly the declared frame length; `MessageLengthUnderDelimiter` not dropping exactly the delimiter to resync; `MessageTooLarge` consuming bytes or growing the buffer before rejecting; byte-conservation drift between fed and consumed bytes; a write/read round trip losing the encoded `id` |

Run a target locally (from inside `fuzz/`, nightly + `cargo-fuzz` required):

```bash
cargo +nightly fuzz run fuzz_udp_flow
cargo +nightly fuzz run fuzz_tcp_clienthello
cargo +nightly fuzz run fuzz_command_channel
```

**CI.** The dedicated `fuzz` job (`.github/workflows/ci.yml`; nightly
toolchain — cargo-fuzz requires it) installs `cargo-fuzz` and runs all four
*originally-wired* targets — `fuzz_frame_parser`, `fuzz_hpack_decoder`,
`fuzz_udp_flow`, and `fuzz_tcp_clienthello` — for 300 s each on every push/PR,
uploading any crash artefacts from `fuzz/artifacts/`. The per-cell pipeline
skips the e2e `fuzz_tests` wrapper to avoid rebuilding the fuzz crate under
every crypto-provider cache; the wrapper's 10 s smoke runs (§2),
`fuzz_tcp_clienthello` included, still cover local `cargo test -p sozu-e2e`
invocations. The daily-scheduled `simulation-sweep.yml` widens all four
targets to `fuzz_seconds` (default 900 s) via its `extended-fuzz` job matrix.
`fuzz_command_channel` is **not yet wired** into either the `fuzz` CI job, the
`extended-fuzz` sweep, or the `e2e/src/tests/fuzz_tests.rs` wrapper — adding a
`fuzz_command_channel` step to all three, mirroring `fuzz_tcp_clienthello`'s,
is proposed but intentionally left for a separate CI-wiring decision. Until
then run it manually with the commands above when touching
`command/src/channel.rs`.

**When to run fuzzers.** Any H2 parser, HPACK, UDP-core, TCP SNI-preread, or
command-channel-framing change must run the focused unit/e2e tests *and* the
relevant cargo-fuzz target before pushing (`CLAUDE.md > Testing`).

---

## 7. E2E conventions

The e2e harness (`e2e/src/`) spawns real Sōzu workers and drives them with mock
clients and backends. The rules below are non-negotiable — each exists because
skipping it produced a real flaky-test or papered-over-bug commit.

- **Never hardcode ports.** Allocate through the port registry
  (`e2e/src/port_registry.rs`). Use `tests::create_local_address()`
  (`e2e/src/tests/tests.rs`), which draws a free localhost port from the
  registry. Hardcoded ports collide under parallel test execution.
- **Always drain with a `loop_read_*` helper when asserting on TCP responses.** A
  single `read()` sees one TCP segment under load — your assertion races the
  network. Use the looping readers (`Client::receive_until_eof`,
  `e2e/src/mock/client.rs`, and the UDP analogue in `e2e/src/mock/udp_client.rs`)
  that drain until EOF or a deadline. Commits exist *only* to paper over this
  rule being skipped — do not add to them.
- **Prefer deadlines / repeat-until-error over `sleep`.** Use
  `repeat_until_error_or` (`e2e/src/tests/mod.rs`) or an explicit deadline for
  timing-sensitive assertions. A fixed `sleep` is both slow and flaky.
  `repeat_until_error_or(n, ..)` is a **stability check, not a retry**: it
  loops while the inner test keeps succeeding and returns `Fail` on the
  first bad trial, so it requires `n` **consecutive** clean runs — pick `n`
  for what the test is trying to prove (a timing/race property that must
  hold every run wants several consecutive passes; a property one clean
  delivery already proves gets no extra assurance from repeating it, only
  more exposure to unrelated per-trial harness flake). All three of its
  outcome lines start with `stability check`, so one
  `grep 'stability check'` over a run log finds the passes, the failures and
  the interrupted checks alike; `n = 1` claims no consecutiveness property it
  does not have — its FAIL and INTERRUPTED lines end `(a single clean run is
  required)`, and its pass line reads `stability check PASSED: the single
  required run succeeded`. See issue #1410.
- **Assert a status by decoding it, never by scanning a field block for its
  digits.** An HPACK block is not text. `payload.windows(3).any(|w| w == b"421")`
  matches any three adjacent bytes, and every Sōzu response carries a `Sozu-Id`
  correlation header (`HttpContext::on_response_headers`,
  `lib/src/protocol/kawa_h1/editor.rs`) whose value is
  the session's 26-character Crockford base-32 ULID — an alphabet holding every
  decimal digit, emitted with the Huffman bit clear, so it lands in the block as
  plain ASCII. A 3-digit needle therefore matches an ordinary 200 about once in
  1400 responses. Worse, the ULID's leading ten characters are a monotonic
  millisecond timestamp, so a hit landing there is not independent between runs:
  it holds for every response generated in a window whose length depends on
  which character triple it occupies — 1 ms for the last, 32 ms, ~1 s, ~33 s and
  ~17.5 min for the middle, and ~9.3 h, ~12.4 d or ~1.1 y for the slow head.
  Two consecutive CI retries red and a re-run green is that signature. That is
  issue #1353 — a test that never read the status at all, presenting as a
  load-dependent flake. Decode the `:status` field RFC 9113 §8.3.2 puts first in
  the block: `decode_status` and `headers_status_matches`
  (`e2e/src/tests/h2_utils.rs`), guarded by
  `h2_status_checks_decode_the_status_field_not_any_matching_bytes`
  (`e2e/src/tests/h2_security_sni.rs`). All three former copies of the scan now
  route through it — `h2_security_sni.rs`, `h2_tests.rs`, and
  `listener_update_tests.rs`. Do not read an OR with an answer-body match as
  mitigation — it *widens* the false-positive surface rather than narrowing it.
  `h2_tests.rs` carried one such disjunct (`""status_code": 404"`); it was
  removed rather than documented, and
  `test_h2_default_answer_terminates_stream` passes on the decoded `:status`
  alone. The H1 form of the rule is a
  status-line prefix check rather than `response.contains("302")`
  (`e2e/src/tests/redirect_rewrite_auth_tests.rs:264`). Nineteen indexed-status
  byte probes survive in `h2_security_tests.rs` and
  `h2_security_header_injection.rs`, inventoried in
  `e2e/COVERAGE.md > Status assertions`, together with the `0x8D` / `:status 404`
  mislabel that has to be settled before converting them.
- **A false positive in a rejection assertion is worse than one in a liveness
  assertion.** #1353 turned a test red. The same scan in
  `try_strict_sni_binding_toggle` (`e2e/src/tests/listener_update_tests.rs`) fed
  `got_rejection_or_421`, which gates the `strict_sni_binding=true` *rejection*
  check, so a ULID false positive there would have turned a security test
  silently **green** — a listener that had stopped rejecting would still pass.
  When auditing a byte-scan assertion, ask which direction its false positive
  points before pricing the fix. Both sites are decoded now.
- **`decode_status` returns `None` on a size-update-prefixed block, and whether
  that is fail-closed depends on the call site.** `H2BlockConverter::emit_pending_size_update_if_new_block`
  (`lib/src/protocol/mux/converter.rs:112`, armed at
  `lib/src/protocol/mux/h2.rs:5254`) prepends a `001xxxxx` HPACK dynamic table
  size update when a peer changes `SETTINGS_HEADER_TABLE_SIZE`, and three e2e
  call sites send one: `h2_security_tests.rs:2440` (value 0) and
  `h2_handshake_chromium_146` (`h2_utils.rs:721`, value 65 536) from
  `h2_correctness_tests.rs:3605` and `:3709`. No test that decodes a `:status`
  sends one, and the three that send one decode no status, so nothing meets the
  update today — `h2_handshake` sends empty SETTINGS. When that changes, `None`
  reads as "no status": fail-closed for a `got_X` asserted positively,
  fail-**open** for one used as `|| !got_X`, which is exactly the shape of
  `got_200` in `try_strict_sni_binding_toggle`. Teach the helper to skip a
  leading update before pointing a new assertion at a size-updating client.
- **Check the direction of a false negative too, not just a false positive.**
  Replacing `payload.contains(&0x88)` with a decode is not automatically a
  strict improvement: the scan had no false negative for a Sōzu 200, the decode
  has one. Whether that is safe depends on whether the flag is asserted or
  negated at its call site, which is recorded in the comment above `got_200`
  (`e2e/src/tests/listener_update_tests.rs`).
- **A test that only reddens under CI load is not automatically a flake — find
  the production site first.** Before retrying or quarantining, ask whether the
  symptom is reachable at all. #1353's 421 has exactly one emission site
  (`lib/src/protocol/mux/mod.rs:1857`), reachable only through
  `RetrieveClusterError::SniAuthorityMismatch`, which is constructed at exactly
  one site (`lib/src/protocol/mux/router.rs:720`) immediately after
  `incr!(names::http::SNI_AUTHORITY_MISMATCH)` — and the failing run reported
  that counter unmoved, alongside a correct backend request count. The proxy was
  innocent by construction, and sixteen serial local reproductions were never
  going to show otherwise. Reading the emission path cost less than the first
  reproduction attempt.
- **Worker-upgrade workflow.** Hot worker upgrades (SCM_RIGHTS fd handoff +
  graceful drain) are covered by `test_upgrade*` in `e2e/src/tests/tests.rs`.
  Read `doc/upgrade_e2e_tests.md` before touching upgrade code and run:

  ```bash
  cargo test -p sozu-e2e test_upgrade
  cargo test -p sozu-e2e test_upgrade_multiple_in_flight -- --nocapture
  ```

- **Mock backends/clients** live in `e2e/src/mock/`: `sync_backend.rs`,
  `async_backend.rs`, `h2_backend.rs`, `raw_h2_response_backend.rs`,
  `udp_backend.rs`, `client.rs`, `udp_client.rs`, `https_client.rs`,
  `aggregator.rs`. Setup helpers (`setup_sync_test`, `setup_async_test`,
  `create_local_address`, `repeat_until_error_or`) are in `e2e/src/tests/mod.rs`
  and `e2e/src/tests/tests.rs`.
- **Know what e2e cannot reach, and write it down.** Some code is unreachable
  from a real worker for structural reasons, so a test aimed at it passes for
  the wrong reason and guards nothing. Before adding a test for a defect on a
  quiet path, confirm a session actually gets there — planting a temporary
  unconditional `panic!` in the target function and running the suite settles
  it in one run. The worked example is `protocol::kawa_h1::Http`: neither
  `HttpStateMachine` (`Expect | Mux | WebSocket`, `lib/src/http.rs`) nor
  `HttpsStateMachine` (`Expect | Handshake | Mux | WebSocket`,
  `lib/src/https.rs`) had a variant holding it and `Http::new` had no code
  caller under either module spelling, so H1 runs through `protocol/mux` and
  the whole session — including `kawa_h1::save_http_status_metric` — was dead
  in every binary. It was deleted on 2026-09-20 (sozu#1346); the unit test
  that guarded it was ported onto the live `mux::stream` path instead of being
  deleted with it. New findings of this kind belong in
  `e2e/COVERAGE.md > Out of e2e reach by construction`, with the mechanism, not
  just the conclusion.
- **A worker's own log output IS readable — name the level.** It was not until
  `Worker::start_new_worker_with_logging` / `start_new_worker_owned_with_logging`
  (`e2e/src/sozu/worker.rs`) and `WorkerLogCapture` (`e2e/src/sozu/log_capture.rs`):
  pass a `file://` target under `tempfile` plus a `parse_logging_spec` level, and
  read the lines back after `wait_for_server_stop` (that join is what flushes the
  backend). The plain `start_new_worker*` entry points are untouched and still log
  to stdout at `error`. Two traps. A HEALTHY session emits no `MUX-*` line at
  `error` — every `log_context!` expansion on a clean path is a `trace!` — so
  raise the level, scoped (`"error,sozu_lib::protocol::mux=trace"`), or provoke an
  error path on purpose. And do NOT reuse the UDP drain from
  `capture_test_logs_at_level` (`lib/src/lib.rs`): it reads only after the run and
  silently drops datagrams at H2 volume, which is a load-sensitive flake, whereas
  a file has no loss mode. Worked example for raising the level:
  `tests::h2_log_context_tests::test_h2_proxy_protocol_peer_is_the_advertised_client`.
  Worked example for provoking instead, which is the only option when EVERY
  expansion of a macro is an `error!` — `log_socket_context!` in
  `lib/src/socket.rs` is:
  `tests::socket_log_context_tests::test_tls_socket_log_peer_is_the_advertised_client`.
  Full cost and residue in
  `e2e/COVERAGE.md > Out of e2e reach by construction`.
- **A clock refactor is not wire-falsifiable; the behaviour it drives is.**
  Changing *which* clock a deadline reads — e.g. the H2 core sampling
  `Context::now` once per `Mux::ready` pass instead of calling `Instant::now()`
  per frame — produces the same elapsed time to within an event-loop pass, so
  every e2e test stays green by construction. Do not write one and claim it
  guards the refactor; inject an instant in a unit test instead. What a wire
  test *can* falsify is the behaviour: that a rate window decays, that a
  deadline fires, that it does not fire early, and that it carries the right
  error code. `e2e/src/tests/h2_clock_tests.rs` covers those four for the H2
  flood window and the RFC 9113 §6.5 SETTINGS-ACK watchdog, and each
  `To SEE THIS RED:` there names a mutation that breaks the behaviour rather
  than the plumbing. Note also that a deadline evaluated only inside
  `readable()` / `writable()` needs an event to be observed: poke the
  connection past the budget, or a quiet socket reads as a missing deadline.
  The mechanism and the mutation table live in
  `e2e/COVERAGE.md > Clock-driven behaviour: what the wire can falsify`.
- **One e2e test exercises external conformance:** `test_h2spec_conformance`
  (`e2e/src/tests/tests.rs`) runs ~145 RFC 9113 scenarios via the `h2spec`
  binary; CI installs it. It skips cleanly when `h2spec` is absent from `PATH`.

---

## 8. Regression guards

A regression guard is a test that *mechanically enforces a convention* so it
cannot rot. The model is `lib/tests/log_layout.rs`:

- It walks every `lib/src/**/*.rs` and asserts that every `error!`/`warn!`/
  `info!`/`debug!`/`trace!` call site carries the canonical log-context envelope
  (a `log_context*!()` macro or `.log_context()` call) within a bidirectional
  window — enforcing the "every protocol log line carries a load-bearing tag"
  rule from `CLAUDE.md > Logging`.
- The scan logic is shared with `lib/build.rs` via `include!`, so the test gate
  and a build-time `cargo:warning=` emitter cannot diverge: drift is *both* a
  failing test and a visible warning on every `cargo build`.
- It ships a `KNOWN_PREEXISTING_VIOLATIONS` allowlist as a pressure-release valve
  for incremental cleanup. The list is currently empty; new sites must use the
  canonical envelope rather than join the allowlist.

When you find yourself writing a comment that says "always do X here", consider
whether a regression guard can enforce X instead. A convention a machine checks
is a convention that stays true.

---

## 9. The `#[ignore]` policy

`#[ignore]` is permitted **only** for:

- an environment dependency the host cannot satisfy (e.g. a test needing an
  external binary or a kernel capability not present in the dev/CI image), or
- a tracked follow-up with a written rationale pointing at the issue.

Rules:

- **An ignored test MUST carry a reason** — the `#[ignore = "..."]` string or an
  adjacent comment explaining *why* and *what unblocks it*. A bare `#[ignore]` is
  not acceptable.
- **`#[ignore]` is never a way to hide a failing test.** A flaky or failing test
  means there is a defect: fix the defect, or remove the test with a written
  rationale in the commit. Suppressing a red test silently is the one thing this
  policy exists to prevent.
- **Prefer graceful runtime skips over `#[ignore]` when the prerequisite is
  detectable at runtime.** `e2e/src/tests/fuzz_tests.rs` probes for the nightly
  toolchain + `cargo-fuzz` and returns clean with a notice when they are absent,
  so the test stays a real `#[test]` that runs the moment the prereqs exist.

---

## 10. What every change must land with

A crisp checklist. See `CONTRIBUTING.md` for the contribution process and CLA;
this is the testing contract layered on top.

- **Protocol / parser / security-sensitive changes ship their full coverage in
  the same changeset.** Unit + property/simulation + fuzz + e2e tests land *with*
  the code, not in a follow-up. The security-sensitive surfaces (TLS/rustls, the
  H2/H1 mux, proxy-protocol, the command channel, metrics/logs — see
  `CLAUDE.md > Security-sensitive areas`) require conservative changes *and*
  tests, full stop.
- **New sans-io / state-machine code carries assertion density.** ≥ 2 meaningful
  assertions per non-trivial function, pre/post-conditions, pair assertions, and
  a `check_invariants()` full sweep at every public entry point (§4). Where the
  core is pure enough, add a deterministic simulation harness following the
  recipe in §5.
- **H2 parser / HPACK / UDP-core changes run the focused e2e tests + the relevant
  cargo-fuzz target** before pushing.
- **Public metric / config-key / CLI-flag changes update their docs in the same
  changeset.** New or renamed metrics update `doc/configure.md`; user-visible
  behaviour updates `CHANGELOG.md` (Keep-a-Changelog). Stale docs are bugs.
- **The full validation chain is green before pushing:** `cargo clippy
  --all-targets -- -D warnings`, `cargo +nightly fmt --all -- --check`, and
  `cargo test --workspace --locked`. No CI clippy/fmt gate runs automatically on
  the per-PR path — run both locally.

If a change touches a network-facing path, ask the TigerStyle question: *what
must always be true here, and what must never be?* Then assert both — and, if the
core is pure, let the simulator try to break it.
