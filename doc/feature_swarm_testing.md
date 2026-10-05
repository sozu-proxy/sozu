# Feature-matrix and swarm testing

Sōzu uses two complementary forms of variation in its test strategy. They
exercise different things and their results must remain separate.

**Cargo feature testing** changes the program that Cargo compiles. It selects a
TLS provider, a logging level, and optional production behavior, then runs the
test suites supported by that compiled graph. This detects build failures,
feature forwarding errors, and interactions between conditionally compiled
paths.

**Swarm testing** varies the operations that a generated workload is allowed
to use. Omitting operations can expose deeper states that are suppressed or
crowded out when every operation competes in every run. Sōzu's deterministic
simulations add seeded replay to that general technique. This is the technique
described by Groce et al. and applied to deterministic Rust workloads by
Pierre Zemb. A Cargo matrix is therefore not itself a swarm test; it is a
complementary compilation axis inspired by the same bounded-variation method.

The counts below define the required test space. They are not evidence that a
campaign has completed. Completion is established only by the campaign report
and its successful cell receipts.

## Normalized product configurations

The Linux product space contains eight factors:

- exactly one effective TLS provider mode per test cell:
  `crypto-ring`, `crypto-aws-lc-rs`, `crypto-openssl`, or `fips`;
- one logging mode: `off`, `debug`, or `trace`;
- six independent Boolean features: `jemallocator`, `opentelemetry`,
  `tolerant-http1-parser`, `simd`, `splice`, and `tui`.

The logging modes are normalized behaviors rather than arbitrary bit sets.
The provider modes are normalized in the same way: `fips` may activate
AWS-LC implementation features internally while remaining the one selected
effective mode.
`off` enables neither logging feature, `debug` enables `logs-debug`, and
`trace` enables `logs-trace`. These cells run in the release profile. In a dev
build, `debug_assertions` activates additional logging code and would blur the
distinction between the three modes.

This normalized Cartesian product contains
`4 providers × 3 logging modes × 2⁶ Boolean combinations = 768`
product configurations. The `unstable` marker is classified separately and
does not multiply this product.

Cargo's requested flags are not sufficient evidence of the compiled result.
Cargo unifies features enabled through dependency paths, and a dependency can
reactivate a default feature even when the selected package uses
`--no-default-features`. Each cell must therefore record and validate its
effective feature graph before running tests. Production graphs must remain
free of harness-only features, while E2E graphs must explicitly contain the
features required by their fixtures.

Logging features apply to each selected package's public feature surface. The
binary and E2E packages explicitly forward their logging mode to both
`sozu-lib` and `sozu-command-lib`. A `sozu-lib`-rooted cell enables that mode
on `sozu-lib` only because its manifest deliberately does not forward the
feature to `sozu-command-lib`; the effective-graph oracle preserves this
package boundary instead of inventing dependency features.

## Bounded CI matrix

The pull-request matrix contains 16 release configurations. It is a covering
array over the four providers, three logging modes, and six Boolean features:

- eight anchors cover the minimum and maximum configuration for each provider;
- four `debug` rows balance every Boolean feature on exactly twice;
- two complementary cross-corners and two deterministic samples add variation
  to the `off` and `trace` rows.

The generator rejects duplicate rows and verifies every pair of factor values.
For example, every provider is exercised with every logging mode, and every
pair of Boolean values occurs somewhere in the matrix. Pairwise coverage does
not prove every three-way or higher-order interaction. It is the bounded CI
budget, not a substitute for the exhaustive local campaign.

The first documented replay seed is `20261005`. Normal CI derives its seed
deterministically from the exact source SHA. Re-running the same SHA therefore
reproduces the same rows byte for byte, while another SHA may choose different
seeded samples without changing the anchors or pairwise requirements.

Package and harness suites receive projections of each product configuration.
Projection removes only axes that the target package cannot accept. In
particular, it must not discard `opentelemetry`, `tolerant-http1-parser`,
`simd`, or `splice` from a package that supports them merely because a current
test observes the feature indirectly. The gRPC suite and the eight real-service
suites use harness features layered onto the projected E2E graph; those
harness features are not additional product axes.

## Exhaustive local campaign

The exhaustive local campaign enumerates all 768 product configurations, then
deduplicates equivalent package projections. Its required cell counts are:

| Suite | Cells |
|---|---:|
| Sōzu binary build and tests | 768 |
| command library | 3 |
| generic E2E | 192 |
| fuzz targets | 6 |
| gRPC E2E | 192 |
| Sōzu library | 192 |
| process-level E2E | 768 |
| eight real protocol services | 1,536 |
| deterministic simulations | 5 |
| TUI process tests | 384 |
| **Total** | **4,046** |

These are 4,046 typed suite-by-projection cells, not 4,046 distinct product
configurations. The five simulation binaries and six fuzz targets are fixed
auxiliary cells. They remain important campaign gates, but they are not each
multiplied by the 768 Cargo configurations and do not make the operational
swarm exhaustive. Their own seeds, operation subsets, durations, and coverage
remain separate evidence.

The runner defaults to one worker and accepts two local core workers, with at
most one serialized Cargo build using four jobs. Each worker has its own Cargo
target, disk-backed `/tmp`, test artifact, process group, and network, mount,
and IPC namespaces. The builder cannot replace a target until the prior test
process group is gone. Generic doctests run afterward in the serialized Cargo
lane with the same profile and features. A measured two-worker pilot completed
two copies of the E2E suite in 438.05 seconds, compared with 875.28 seconds for
the same work serially. Its sampled aggregate RSS peaked at 2,257,128 KiB; the
two separate GNU time maxima were 1,124,508 and 1,114,344 KiB. This result
qualifies two core workers only and is not an estimate for the full campaign.

Real-service cells remain serialized in the host network namespace because
their Docker backends are published on host loopback. This avoids competition
between campaign configurations; it does not claim that unrelated host
processes cannot share a resource. Service fixtures may clean up only resources
that carry the current run's ownership identity.

## Cell evidence and resumability

A cell is eligible to run only after two checks:

1. its effective Cargo feature graph matches the requested projection;
2. every selected test command has a non-empty inventory, including the
   expected feature-gated tests.

This prevents a green compile from hiding a re-enabled default provider and
prevents a successful zero-test selection from being counted as coverage.
Feature-gated inventory is checked in both the serial and prepared-worker
paths. The H2 PROXY-peer oracle remains present in every E2E cell: it checks
the access-log address in every build and adds the detailed `MUX-H2` peer-slot
checks when `debug_assertions` or `logs-trace` compiles those trace events.

Each terminal outcome is bound to the source fingerprint, `Cargo.lock`, matrix
generator, normalized commands, effective feature graph, test inventory,
configuration, suite, and the toolchain that actually runs it. Stable cells
record stable Rust and Cargo. Fuzz cells additionally record nightly Rust,
nightly Cargo, and `cargo-fuzz`. The runner revalidates the source before work
and again before every terminal credit; source drift leaves the active claim
and all remaining cells uncredited. Receipts are written atomically only after
the cell exits successfully; a separate atomic outcome records a terminal
failure. A normal resume reuses both exact successes and exact failures, so a
known red cell is not retried silently. `--replay-failed` explicitly reruns
failed cells. Missing, truncated, interrupted, or differently fingerprinted
evidence is never reusable. An orphaned claim remains blocked whenever its
recorded process group is alive or cannot be reconciled exactly; the runner
neither launches a duplicate nor kills an unproven process.
The checkout-wide orchestrator lease uses a stable, UID-private host path that
does not follow a worker's `TMPDIR`, so private worker directories cannot split
the lock domain.

The final report distinguishes execution from success. `all_executed` becomes
true only when every selected cell has a terminal `success` or `failed` state;
`all_passed` additionally requires zero failures. The command exits non-zero
when either condition is false. A failure does not stop later cells: each
command and cell keeps its own exit status and compressed log.

Every command has a mandatory supervisor deadline, 1,800 seconds by default
and configurable with `--command-timeout-seconds`. Expiry records exit 124,
terminates only the attempt's recorded process group, and lets later safe cells
run. A command that exits while leaving a live descendant records a failure;
the claim is never removed until its process group is empty. Failure to prove
or perform that exact cleanup blocks the campaign without a terminal receipt.
Each protocol-service attempt also gets a unique Docker ownership label, so a
different worktree running the same projected cell cannot be selected by its
cleanup.

The exhaustive runner checks disk, memory, swap, and one-minute load before
admission. Later batches repeat the disk, memory, and swap checks, while merely
recording load: the one-minute load average includes work from the immediately
preceding owned batch and is not an independent between-batch admission signal.
A failed capacity check leaves remaining cells `not-run` and preserves all
receipts. Do not wrap the campaign in `Restart=on-failure`; an entirely executed
campaign with known red cells intentionally exits non-zero.

Failures are associated with issues in a separate triage index keyed by the
exact receipt identity. Editing that index never changes the source, command,
or receipt fingerprint. Reusing one issue for several cells requires verified
evidence that they share a cause; matching error text alone is insufficient.

## Commands

Validate the feature catalogue, projections, and bounded covering array:

```console
$ python3 .github/scripts/feature_matrix.py --seed 20261005 validate --repo-root .
```

These scripts require Python 3.11 or newer and use only the standard library.
The workflows install Python 3.12 explicitly.

Inspect the 16 CI rows or all 4,046 exhaustive cells without writing a
receipt:

```console
$ python3 .github/scripts/feature_matrix.py --seed 20261005 matrix core
$ python3 .github/scripts/run_feature_matrix.py --mode exhaustive \
    --seed 20261005 --repo-root . \
    --state-dir "$HOME/sozu-feature-swarm-state" --dry-run
```

Run or resume the local exhaustive campaign. The state directory must be
outside the checkout and on storage that satisfies the runner's capacity
gate; do not place Cargo targets under `/tmp`:

```console
$ python3 .github/scripts/run_feature_matrix.py --mode exhaustive \
    --seed 20261005 --repo-root . \
    --state-dir "$HOME/sozu-feature-swarm-state" --jobs 4 --workers 2 \
    --command-timeout-seconds 1800
```

Replaying one failed cell uses the same source, seed, state directory, and
suite selection, plus its exact cell identifier:

```console
$ python3 .github/scripts/run_feature_matrix.py --mode exhaustive \
    --seed 20261005 --repo-root . \
    --state-dir "$HOME/sozu-feature-swarm-state" --jobs 4 \
    --suites e2e --cell e2e/ring-off-0000 --replay-failed
```

The final identifier is an example shape; copy the actual identifier from the
campaign report. Known release-profile failures are tracked in
[#1860](https://github.com/sozu-proxy/sozu/issues/1860) and
[#1861](https://github.com/sozu-proxy/sozu/issues/1861). They remain failures:
triage links do not change receipts, exit status, or `all_passed`.

## References

- Pierre Zemb, [Designing Rust FDB Workloads That Actually Find Bugs](https://pierrezemb.fr/posts/writing-rust-fdb-workloads-that-find-bugs/).
- Alex Groce, Chaoqiang Zhang, Eric Eide, Yang Chen, and John Regehr,
  [Swarm Testing](https://users.cs.utah.edu/~regehr/papers/swarm12.pdf),
  ISSTA 2012.
- The Cargo Book, [Features](https://doc.rust-lang.org/cargo/reference/features.html),
  especially default features, feature unification, and inspection of resolved
  feature graphs.
