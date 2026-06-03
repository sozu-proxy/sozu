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
  FoundationDB approach (seeded RNG, virtual clock, in-process network) to Rust;
  it is the inspiration for Sōzu's harness. Sōzu does not depend on it: the
  `sozu-lib` no-`async fn` rule and the synchronous sans-io `UdpManager` make a
  direct seeded loop (clock + seed injected as plain arguments) the right fit
  rather than `moonpool-sim`'s async provider bundle.
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
| **Fuzz** | `fuzz/fuzz_targets/*` (out-of-workspace `sozu-fuzz` crate) | nightly toolchain + `cargo-fuzz` | nightly `fuzz` CI job; `#[ignore]`-style runtime skip when prereqs absent | smoke per-PR, real fuzzing nightly |
| **Deterministic simulation** | `lib/tests/udp_simulation.rs` | nothing (pure core, no I/O) | runs in `cargo test --workspace`; widened via env knobs | yes (256-seed sweep) |
| **Regression guards** | `lib/tests/log_layout.rs` | nothing | runs in `cargo test -p sozu-lib`; build-time `cargo:warning=` echo from `lib/build.rs` | yes |

Notes:

- The e2e suite includes H1/H2/TLS/TCP/UDP coverage plus targeted security and
  feature suites; see `e2e/src/tests/mod.rs` for the full module list
  (`h2_tests`, `h2_security_*`, `mux_tests`, `tls_tests`, `tcp_tests`,
  `udp_tests`, `command_channel_security_tests`, `listener_update_tests`, …).
- `e2e/src/tests/fuzz_tests.rs` is a thin integration wrapper that shells out to
  the three fuzz targets for 10 s each. It *skips gracefully* (prints a notice,
  returns clean) when the nightly toolchain or `cargo-fuzz` is missing, so the
  rest of the e2e suite still runs. CI skips it in the per-cell pipeline (`--skip
  tests::fuzz_tests::`) and runs real fuzzing in the dedicated nightly job.

---

## 3. Running the suites

All commands assume `protoc` is installed and the `1.88.0` toolchain pinned by
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
# Unit + lib-level tests (includes the UDP simulation + log-layout guard):
cargo test -p sozu-lib --locked

# A single test module / filter:
cargo test -p sozu-e2e -- h2_              # all H2 e2e tests
cargo test -p sozu-e2e -- test_udp_        # all UDP e2e tests
cargo test -p sozu-e2e test_upgrade        # worker-upgrade e2e (see doc/upgrade_e2e_tests.md)

# Deterministic UDP simulation:
cargo test -p sozu-lib --test udp_simulation        # the default 256-seed sweep
```

### Simulation sweep + single-seed replay

The simulator reads three env knobs (`lib/tests/udp_simulation.rs`,
`doc/udp_simulation.md`):

| env var | effect |
|---|---|
| `SOZU_UDP_SIM_SEED` | run that ONE seed verbosely (decimal or `0x`-hex) — the replay path |
| `SOZU_UDP_SIM_SEEDS` | sweep `0..n` seeds instead of the default `0..256` |
| `SOZU_UDP_SIM_STEPS` | run `n` steps per seed instead of the default `3000` |

```bash
# Reproduce a CI failure exactly (the panic prints the failing seed + step):
SOZU_UDP_SIM_SEED=0xdeadbeef cargo test -p sozu-lib --test udp_simulation

# Widen / deepen the sweep (FoundationDB nightly seed-swarm analog):
SOZU_UDP_SIM_SEEDS=1024 SOZU_UDP_SIM_STEPS=5000 \
  cargo test -p sozu-lib --test udp_simulation udp_simulation_seed_sweep -- --nocapture
```

### Fuzzing

```bash
# From inside fuzz/ (requires cargo-fuzz + nightly):
cargo +nightly fuzz run fuzz_frame_parser
cargo +nightly fuzz run fuzz_hpack_decoder
cargo +nightly fuzz run fuzz_udp_flow

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
`check_invariants()` sweep. The invariants `check_invariants()` enforces
(`manager.rs:683`):

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

`lib/tests/udp_simulation.rs` is a VOPR/FoundationDB-style deterministic
simulation of the sans-io UDP core. The full design is in
`doc/udp_simulation.md`; the essentials:

**How it works.** A single seeded `StdRng` drives everything. A base `Instant` is
captured once *outside* the stepping loop and advanced only by RNG-drawn deltas —
no wall-clock read happens inside the loop, so a run is a pure function of its
seed. Each step draws one weighted-random action from an adversarial grammar
(client/backend datagrams, backend resolutions, clock advances, reconfig storms,
cap shrinks below the live count, aborts, drains, mass teardown). After **every**
action the harness fully drains `poll_output()` to `None`, folding outputs into a
shadow model that tracks active-flow accounting.

**Invariants checked.** Two layers fire on every step:

- The core's own `debug_assert` invariants (the seven in §4) fire for free inside
  each `handle_*` call.
- The harness adds model-level invariants after each fully-drained step:
  `flow_count()` never exceeds the high-water mark of any cap ever set;
  `poll_output()` is `None` after draining; `poll_timeout()` is coherent with
  `flow_count()`; **model balance** — `created_seen − evicted_seen ==
  flow_count()` (no underflow, no leak); and no panic for any input. A **final**
  giant-clock-jump + `close_all` must drain the manager to zero
  (`flow_count() == 0`, `poll_timeout() == None`, `created == evicted`).

**Buggify.** `buggify(rng, p)` is the FoundationDB `buggify` analog: with low
per-call probability it injects an *extra* adversarial event (a stale
`BackendResolved`, a reconfig burst, a `max_flows` shrink to a tiny value, a
giant clock jump). It runs only under simulation, never in production.

**Replay ergonomics.** On any failure the panic message carries the failing
**seed + step**, so the exact run reproduces with
`SOZU_UDP_SIM_SEED=<seed>` (verbose, single-seed). The sweep widens with
`SOZU_UDP_SIM_SEEDS` / `SOZU_UDP_SIM_STEPS` for the nightly seed-swarm pattern.
A separate `udp_simulation_is_deterministic` test asserts that the same seed
yields a bit-identical trace, guarding against a hidden nondeterministic
dependency (wall clock, `rand`, hash-map iteration order) leaking into outputs.

### Recipe: adding a simulator over another sans-io core

The pattern generalizes to any pure state machine. To add one:

1. **Make the core sans-io.** Inject the clock (`now: Instant`) and any hash seed
   at construction; remove `Instant::now()` and `rand` from the datapath.
2. **Pack it with assertions** plus a `check_invariants()` full sweep at the end
   of every public mutating entry point (§4).
3. **Write a seeded driver** in `lib/tests/`: one `StdRng`, a base `Instant`
   advanced only by drawn deltas, a weighted action grammar that biases toward
   the adversarial transitions (reconfig, cap changes, teardown), and a shadow
   model for cross-checking observable outputs.
4. **Add `buggify`-style fault injection** for low-probability extra adversarial
   events.
5. **Print the seed + step on failure** and wire the same `SOZU_*_SIM_SEED /
   _SEEDS / _STEPS` replay knobs.

The H2 mux (`lib/src/protocol/mux/`) is the obvious next candidate — its stream
slot lifecycle, flow-control accounting, and GOAWAY/RST_STREAM semantics are a
state machine of the same shape. This is a stated direction, not a commitment.

---

## 6. Fuzzing

The out-of-workspace `sozu-fuzz` crate (`fuzz/`) has three cargo-fuzz targets
(`fuzz/fuzz_targets/`), each defending a network-facing parser or state machine:

| Target | Surface | Defends against |
|---|---|---|
| `fuzz_frame_parser` | H2 frame parser (`protocol::mux::parser`, RFC 9113 §6) | length-confusion / framing CVEs (`ensure_frame_size!`); parser must reject via `H2Error`/`nom::Err`, never panic |
| `fuzz_hpack_decoder` | HPACK decoder (RFC 7541, `loona-hpack`) under three dynamic-table profiles | header-block oversize and incomplete-update flaws; resize/eviction paths |
| `fuzz_udp_flow` | the sans-io UDP core + flow-key extraction + PPv2 DGRAM framing | flow-count overrun, gauge underflow, fd/slab leak; reuses the same invariants the simulator asserts |

Run a target locally (from inside `fuzz/`, nightly + `cargo-fuzz` required):

```bash
cargo +nightly fuzz run fuzz_udp_flow
```

**CI.** The dedicated nightly `fuzz` job (`.github/workflows/ci.yml`) installs
`cargo-fuzz` on the nightly toolchain and runs `fuzz_frame_parser` and
`fuzz_hpack_decoder` for 300 s each, uploading any crash artefacts from
`fuzz/artifacts/`. The per-cell pipeline skips the e2e `fuzz_tests` wrapper to
avoid rebuilding the fuzz crate under every crypto-provider cache.

**When to run fuzzers.** Any H2 parser, HPACK, or UDP-core change must run the
focused e2e tests *and* the relevant cargo-fuzz target before pushing
(`CLAUDE.md > Testing`).

---

## 7. E2E conventions

The e2e harness (`e2e/src/`) spawns real Sōzu workers and drives them with mock
clients and backends. The rules below are non-negotiable — each exists because
skipping it produced a real flaky-test or papered-over-bug commit.

- **Never hardcode ports.** Allocate through the port registry
  (`e2e/src/port_registry.rs`). Use `tests::create_local_address()`
  (`e2e/src/tests/tests.rs:85`), which draws a free localhost port from the
  registry. Hardcoded ports collide under parallel test execution.
- **Always drain with a `loop_read_*` helper when asserting on TCP responses.** A
  single `read()` sees one TCP segment under load — your assertion races the
  network. Use the looping readers (`Client::receive_until_eof`,
  `e2e/src/mock/client.rs:136`, and the UDP analogue in `e2e/src/mock/udp_client.rs`)
  that drain until EOF or a deadline. Commits exist *only* to paper over this
  rule being skipped — do not add to them.
- **Prefer deadlines / repeat-until-error over `sleep`.** Use
  `repeat_until_error_or` (`e2e/src/tests/mod.rs:232`) or an explicit deadline for
  timing-sensitive assertions. A fixed `sleep` is both slow and flaky.
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
