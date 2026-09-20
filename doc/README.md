# Sōzu

## What is Sōzu?

Sōzu is a reverse proxy for load balancing, written in Rust. Its main job is to balance inbound requests across two or more clusters backends to spread the load.

* It serves as a termination point for TLS sessions. So the workload of dealing with the encryption is offloaded from the backend.

* It can protect the backends by preventing direct access from the network.

* It returns some metrics related to the traffic between clients and backends clusters behind it.

## Introduction

* [Getting started][gs]

* [Configure Sōzu][cg]

* [Configure the Sōzu CLI][cgcli]

* [How to use it][hw]

* [Why you should use Sōzu][ws]

* [Design Motivation][dm]

* [Recipes][r]

## Overview

* [Architecture Overview][ar]

* [Tools & Libraries][tl]

* [Lexicon][lx]

## Operating Sōzu

* [Configure Sōzu][cg]

* [Admin operations & worked examples][cao]

* [Observability — logs, metrics, audit log][ob]

* [Debugging strategies][ds]

* [Benchmarks][bm]

* [Rate limit design][rl]

## Going deeper

* [H2 Mux Internals][h2] — Developer reference for the HTTP/2 multiplexer implementation

* [H2 Mux LIFECYCLE.md][h2lc] — In-tree state-machine reference, maintained alongside the code

* [UDP LIFECYCLE.md][udplc] — In-tree flow/state-machine reference for the UDP datapath (userland conntrack, NAT return, teardown, hardening), maintained alongside the code

* [Health Checks][hc]

* [Lifetime of a session][li]

## Testing

* [Testing guide][tst] — the testing doctrine: assertion-first + deterministic simulation, categories, and what every change must land with

* [Worker upgrade e2e tests][ue]

* [Deterministic simulation (UDP)][uds] — FoundationDB/VOPR-style seeded fault injection for the sans-io UDP core

* [Nightly CI notes][nci]

## Citing code from these documents

These documents anchor their claims to code. There are two forms, and the choice between them is not
stylistic:

* **The prose names an item** — a function, method, struct, enum, field, constant or macro — so cite
  the *symbol*, qualified as `Type::method` so it stays greppable, with the file path and no line
  number: `TcpSession::effective_session_address` (`lib/src/tcp.rs`). A symbol survives every
  edit above it.
* **The prose means a specific statement or branch inside an item** — one `match` arm, one guard, one
  log line — so cite a line or a range: `lib/src/tcp.rs:NNN-MMM, NNN-MMM`. Keep the path
  repo-root-relative; a bare `manager.rs` is ambiguous in this tree.

A line number carries no anchor. It rots the moment anyone edits the file it points into, and the
pull request that breaks it is almost never the pull request that contains it — so no reviewer is
ever shown both halves. In [sozu-proxy/sozu#1335][cit] an audit of one module document found 29 of
its 35 citations wrong: single lines uniformly off by +1 after a `//!` module-doc block was inserted
above them, and ranges off by +38 to +57 after a `debug_assert!` campaign grew the functions. A range
that drifts 46 lines does not mislead slightly — it lands the reader in a different branch.

### Running the resolver locally

The surviving line citations are guarded by the `Doc citations` CI job, which runs on the merge
result. The same check runs locally:

```bash
python3 .github/scripts/check_doc_citations.py             # check the tree
python3 .github/scripts/check_doc_citations.py --show      # print every resolved target line
python3 .github/scripts/check_doc_citations.py --self-test # prove it still fails on a broken fixture
```

It scans `doc/**` and every `**/LIFECYCLE.md`, and fails when a cited file does not exist, a cited
basename is ambiguous, a line number is below 1 or past end-of-file, either end of a range is blank,
a range is inverted, or the same line repeats inside one citation group — `file.rs:NNN/NNN`, which is
what a `/` or `,` continuation renumbered on one half only looks like, and which would otherwise
resolve perfectly.

`--self-test` is what keeps the checker honest. It asserts the exact failures a deliberately broken
fixture must produce, *and* runs the real command line in a subprocess to require exit `1` on that
fixture and exit `0` on a clean one — because reporting a failure and acting on it are two different
lines of code, and a checker that did the first and not the second would be green forever.

### What the resolver does not catch

This matters more than what it does. The resolver cannot tell whether a citation lands on *the
construct the surrounding prose is talking about*. A citation that drifted from line 118 to line 164
still resolves, still hits code, and still passes — and that is the dominant failure mode, not the
exotic one. Measured on the module `LIFECYCLE.md` files: the resolver flagged 27 of the 507 anchors
those documents carry, while the hand audit that followed cut them to 238 and had to renumber 159 of
the survivors.

Two narrower gaps are deliberate. Only the two **ends** of a range are required to be non-blank:
interior blank lines are normal in a span that covers a whole branch — 26 of them across the
guarded surface once the module `LIFECYCLE.md` repair has landed, 57 before it — so requiring every
line would reject correct citations. And a cited path binds
to the citing document's own directory before the repository root, which is what lets a module
`LIFECYCLE.md` write `h2.rs:NNN` for its own sibling; without it the guarded surface reports 251
false ambiguities. The cost is that a sibling could shadow a repo-root file of the same relative
path and hide a real failure. No such pair exists in the tree today, but it is the reason to write
the repo-root-relative path whenever a citation leaves its own module.

A green `Doc citations` run means "no citation is obviously dead". It does not mean the citations are
right, and it is not a licence to skip reading the code when you touch one. Where the prose names an
item, cite the symbol and the question does not arise.

[cit]: https://github.com/sozu-proxy/sozu/issues/1335

## Release Notes

* [Changelog](../CHANGELOG.md)

## Presentations & Slides

* [Sōzu, a hot reconfigurable reverse HTTP proxy by Geoffroy Couprie](https://youtu.be/y4NdVW9sHtU)

* [(FR) Refondre le reverse proxy en 2017 pour faire de l’immutable infrastructure. by Quentin Adam](https://youtu.be/uv3BG1J8YKc)

[gs]: ./getting_started.md
[cg]: ./configure.md
[cgcli]: ./configure_cli.md
[cao]: ./configure_admin_ops.md
[hw]: ./how_to_use.md
[dm]: ./design_motivation.md
[ar]: ./architecture.md
[tl]: ./tools_libraries.md
[lx]: ./lexicon.md
[ws]: ./why_you_should_use.md
[r]: ./recipes.md
[h2]: ./h2_mux_internals.md
[h2lc]: ../lib/src/protocol/mux/LIFECYCLE.md
[udplc]: ../lib/src/protocol/udp/LIFECYCLE.md
[hc]: ./health_checks.md
[li]: ./lifetime_of_a_session.md
[tst]: ./testing.md
[ue]: ./upgrade_e2e_tests.md
[uds]: ./udp_simulation.md
[ob]: ./observability.md
[ds]: ./debugging_strategies.md
[bm]: ./benchmark.md
[rl]: ./rate-limit-design.md
[nci]: ./nightly-ci-notes.md
