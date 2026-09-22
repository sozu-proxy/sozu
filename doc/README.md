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

These documents anchor their claims to code, and to each other. There are three forms, and the
choice between them is not stylistic:

* **The prose names an item** — a function, method, struct, enum, field, constant or macro — so cite
  the *symbol*, qualified as `Type::method` so it stays greppable, with the file path and no line
  number: `TcpSession::effective_session_address` (`lib/src/tcp.rs`). A symbol survives every
  edit above it.
* **The prose means a specific statement or branch inside an item** — one `match` arm, one guard, one
  log line — so cite a line or a range: `lib/src/tcp.rs:NNN-MMM, NNN-MMM`. Keep the path
  repo-root-relative; a bare `manager.rs` is ambiguous in this tree.
* **The prose QUOTES code in a fenced block** — so put the citation in the fence's info string and
  let the checker compare the quote to its source, modulo leading and trailing whitespace. See
  [Pinning a quoted code block](#pinning-a-quoted-code-block).

A line number carries no anchor. It rots the moment anyone edits the file it points into, and the
pull request that breaks it is almost never the pull request that contains it — so no reviewer is
ever shown both halves. In [sozu-proxy/sozu#1335][cit] an audit of one module document found 29 of
its 35 citations wrong: single lines uniformly off by +1 after a `//!` module-doc block was inserted
above them, and ranges off by +38 to +57 after a `debug_assert!` campaign grew the functions. A range
that drifts 46 lines does not mislead slightly — it lands the reader in a different branch.

### Citing another document

A cited path may be a `.md` as well as a `.rs`, and the resolver treats the two identically — same
three rules, no exemption. Prose cites prose all the time: an operations runbook points at the
reference table that defines the knob it is telling the operator to turn, exactly as it points at
the function that implements it. Prefer an anchor (`configure.md#h2-flood-detection-thresholds`)
wherever a whole section is meant, for the same reason a symbol beats a line number in code; keep a
line only where the prose means one specific row or statement, and then cite the row that carries
the claim rather than the example that repeats the value. Prose that says "the catalogue" means a
whole section, so it takes the anchor and stops depending on line arithmetic altogether; a line is
for the case where one row carries the claim, and then it is the row that carries it rather than
the example repeating its value. `doc/configure_admin_ops.md` is the worked case — three citations
into `configure.md`, one anchor for the section and two lines for the two rows.

Until [sozu-proxy/sozu#1444][md-cit] the resolver did not see that form at all. Its pattern matched
`.rs` alone, so a markdown target was never extracted from the document in the first place, and the
class it left unguarded was 100% wrong: at main `95dee167` three citation sites carried six line
targets into `configure.md`, and all six landed on unrelated prose — a `secp384r1` cipher-suite row,
an `sni_preread_timeout` TOML block, a sentence about gRPC backends — while the `.rs` citations in
the same file were accurate. One of the six was made *more* precisely wrong by tooling: sozu#1437
moved that site from line 933 of `configure.md` to line 979 when it shifted lines in that document,
faithfully tracking a target that had been wrong since the day it was written. A re-anchor preserves
the pointer, not the claim. (Written without the `path:line` form on purpose — this document is
inside the guarded surface, so a worked example spelt that way would be resolved as a citation, and
an illustration of a wrong citation would become one.)

### Running the resolver locally

The surviving line citations are guarded by the `Doc citations` CI job, which runs on the merge
result. The same check runs locally:

```bash
python3 .github/scripts/check_doc_citations.py                 # check the tree
python3 .github/scripts/check_doc_citations.py --base main     # + report every citation that drifted
python3 .github/scripts/check_doc_citations.py --show          # print every resolved citation
python3 .github/scripts/check_doc_citations.py --self-test     # prove it still fails on a broken fixture
```

It scans `doc/**` and every `**/LIFECYCLE.md`, resolves every `file.rs:NNN` and `file.md:NNN` in
them, and fails when a cited file does not exist, a cited basename is ambiguous, a line number is
below 1 or past end-of-file, either end of a range is blank, a range is inverted, or the same line
repeats inside one citation group — `file.rs:NNN/NNN`, which is what a `/` or `,` continuation
renumbered on one half only looks like, and which would otherwise resolve perfectly.

A cited path is resolved against the citing document's own directory first, then repo-root-relative,
then as a unique tree-wide suffix. So `configure.md:NNN` in `doc/configure_admin_ops.md` binds to
`doc/configure.md` and is well-formed, exactly as a module `LIFECYCLE.md` cites its siblings by bare
name — but write it repo-root-relative anyway, because a sibling that shadows a repo-root file of
the same relative path would hide a genuine failure.

### Reporting a citation that drifted

Everything above is a floor: a citation that slides onto a *different non-blank line* resolves, hits
code, and passes. That is the dominant failure mode, not the exotic one. In
[sozu-proxy/sozu#1389][drift] a changeset that grew `kawa_h1/editor.rs` by 21 lines and
`mux/router.rs` by 14 moved **24** citations, and the blank-line rule reported **2** — the other 22
all landed on code. Worse, the green job read as "the citations are right" when it only ever meant
"no citation landed on a blank line".

`--base <revision>` closes that, and needs no data the merge already has. Every citation is resolved
the same way — the document's own directory first, so a module `LIFECYCLE.md` keeps citing its
siblings by bare name — and the *text* of the cited line is read at the merge base of `<revision>`
and HEAD as well as at HEAD. Different text is reported, blank or not, at both ends of a range. A
citation this changeset **re-anchored** is not reported: only one that carries the same path and the
same line numbers it carried at the base while the text underneath it changed. Comparison is on the
stripped line, so a re-indent is not drift.

The `Doc citations` CI job passes the pull request's base sha, and checks out with `fetch-depth: 0`
because the default shallow checkout has no base commit to read. An unreachable `--base` is an
**error and exit 1**, never a skip — a guard that answered a missing base with a clean run would be
green forever while comparing nothing. With no `--base` at all, the run says on its own line that
the rule did not run.

It remains a floor in one direction: a citation into code that this changeset never touched is not
compared. Cite a symbol wherever the prose names an item.

`--self-test` is what keeps the checker honest. It asserts the exact failures a deliberately broken
fixture must produce, *and* runs the real command line in a subprocess to require exit `1` on that
fixture and exit `0` on a clean one — because reporting a failure and acting on it are two different
lines of code, and a checker that did the first and not the second would be green forever. The drift
half is asserted the same way and in both directions: `testdata/citations/` carries a document whose
every citation resolves to a non-blank line at **both** revisions, so the same tree must exit `0`
without `--base` and `1` with it, and an unreachable base must be refused rather than skipped.

### Citing a test by name

The same command carries a second, independent rule, for the citation form that has no path at all:
prose naming a **test** as its evidence — "`<name>` pinned the defect", "see `<name>` for the exact
semantics". That form rots the same way a line number does, and more quietly: in
[sozu-proxy/sozu#1380][test-cit] three test names were cited as evidence in six places — in
`CHANGELOG.md`, in `lib/src/router/mod.rs`, in `lib/src/tcp.rs` and in an e2e module preamble — and
none of the three had a definition anywhere in the repository.

The rule is: a backticked identifier that looks like a test name, in prose that is talking about
tests, must name a `fn` somewhere in the tree. "Looks like a test name" is two measured filters —
at least five underscore-separated segments, so a sentence rather than a noun phrase, and the
enclosing comment block or markdown paragraph containing the word "test". Without both, the raw
candidate set is 358 identifiers over 864 sites, nearly all configuration keys and struct fields;
with both it is twelve. The scanned surface is every `*.rs` comment plus `CHANGELOG.md`, `doc/**`
and every `**/LIFECYCLE.md`.

When it fires, repoint the citation at the test that exists, write the test the prose claims, or
drop the claim. Two dispositions are available in the script and both are deliberate, reviewed
decisions rather than escapes:

* `RENAMED_TESTS` — a test cited on purpose by a name it no longer carries, because the prose is
  recording the rename ("it is now `X`"). The entry gives the name it carries now, and **that name
  must itself resolve to a `fn`**, so the forwarding pointer cannot rot in turn.
* `NOT_A_TEST` — a sentence-shaped identifier that is not a test name at all: a configuration key, a
  std method, the identifier of a note kept outside the repository. Each entry carries its reason;
  one without a reason is an unreviewed silencing of the rule.

[test-cit]: https://github.com/sozu-proxy/sozu/issues/1380

### Pinning a quoted code block

The three rules above all check a *pointer* — a path, a line number, a name. None of them looks at
the code a document **quotes**. [sozu-proxy/sozu#1424][quote] measured what that costs:
`doc/h2_mux_internals.md` carried fenced Rust blocks writing `self.encoder`, `self.decoder`,
`self.converter_buf` and `self.lowercase_buf` on `ConnectionH2` months after [#1403][hpack] moved all
four behind `hpack_state.rs`, where they are private. Every one would fail to compile with `E0609`,
and every check here was green over them. A stale quote sitting beside a *resolving* citation reads
as authoritative — a reader who copies it assumes the compile error is theirs.

A fenced block closes that by naming the lines it quotes, in the info string, after the language:

````markdown
```rust lib/src/protocol/mux/hpack_state.rs:121-131
pub(super) fn shrink_converter_buffers(&mut self) {
    if self.converter_buf.capacity() > 16_384 {
        self.converter_buf.shrink_to(4096);
    }
    ...
}
```
````

The checker reads those lines and compares them to the block, line by line. Comparison is on the
**stripped** line, matching the drift rule, so the indentation a quote loses when it leaves an `impl`
block is not a mismatch — every other character is. A citation may carry several spans
(`lib/src/protocol/mux/hpack_state.rs:121-131, 139`), and the block must then be their concatenation
in order. There is no elision
syntax, on purpose.

Because the fence line is part of the document body, the annotation is an ordinary citation — with
one wrinkle worth knowing. The resolver range-checks a pinned block from the moment it lands, but
the drift rule only compares it **from the next commit onward**: it exempts a citation that the base
revision of its own document did not carry, and on the commit that introduces a pin, every pin is
absent. Measured when the seventeen pins in `doc/h2_mux_internals.md` landed — the cited-line total
went 219 to 238 while the compared count stayed at exactly 325, so every one of them was exempt that
day. Renumbering a pin's spans later re-exempts it the same way. Rule 1 still range-checks the new
span and rule 4 still holds the quote to it, so the hole is narrow, but it is the same re-anchoring
the drift rule accepts everywhere else and it is not closed here.

**A pin guards the quote, not the claim beside it.** This is the limit worth internalising before
any of the others. Rule 4 proves that the lines between the fences still match the lines they name;
it has nothing to say about the sentence above them, and a green run says nothing about whether the
prose explains the code correctly. The failure is not hypothetical: the review of the changeset that
introduced this rule found five wrong explanations sitting immediately beside blocks the checker was
certifying byte-exact and exiting 0 over — a fabricated borrow-checker rationale, a miscounted set
of fields, an `any`/`each` inversion, a wrong call-site count, and a helper attributed to the wrong
file. Pinning a block makes the page *look* more trustworthy while leaving that class untouched, so
a reviewer must read the prose against the source exactly as before. The pin buys one thing only,
and it is worth having: the quote cannot silently stop being the code.

**The rule is opt-in, and that is the design, not an oversight.** Two alternatives were measured on
this tree first:

* *Compare every `rust` block against the tree.* The guarded surface holds 215 fenced blocks, 27 of
  them Rust. Searching the whole tree for each block's exact line sequence locates 7. The other 20
  are not stale — they are abbreviated on purpose (`pub fn readable(&mut self, context, endpoint) ->
  MuxResult` drops the generics and the `where` clause that would bury the point), composite (one
  block narrating two distant call sites), or simplified (`pub struct FlowKey { pub src: SocketAddr
  }`). A rule that fires on 20 of 27 blocks is a rule that gets silenced, and a silenced rule is
  worse than no rule.
* *`rustdoc` doctests, via `#![doc = include_str!("../../doc/....md")]`.* This tree has no
  `include_str!` of a markdown file and no doctest configuration anywhere, and the reason is
  structural rather than historical: a doctest is compiled as a **separate crate against the public
  API**. Every snippet at issue sits inside `impl ConnectionH2` and reads a private field of a
  private type in a private module. No doctest can see any of it. The mechanism cannot reach the
  exact class that motivated the rule.

So a block that cannot be quoted verbatim simply carries no annotation and is never compared, which
is what keeps the rule off the illustrative majority. The cost is real and worth stating: a quote
nobody pins is a quote nobody checks. What pinning buys is that the repair is durable — the next
refactor to move those lines is reported, instead of being found two extraction steps later.

[quote]: https://github.com/sozu-proxy/sozu/issues/1424
[hpack]: https://github.com/sozu-proxy/sozu/pull/1403

### What the resolver does not catch

This matters more than what it does. The resolver cannot tell whether a citation lands on *the
construct the surrounding prose is talking about*. A citation that drifted from line 118 to line 164
still resolves, still hits code, and still passes the blank-line rule — and that is the dominant
failure mode, not the exotic one. Measured on the module `LIFECYCLE.md` files: the resolver flagged
27 of the 507 anchors those documents carry, while the hand audit that followed cut them to 238 and
had to renumber 159 of the survivors.

The drift rule above closes that for every line the changeset under test actually moved, which is
where a citation rots. It closes nothing for a line nobody touched: a citation that was already
pointing at the wrong place before this pull request opened is carried forward unreported. [#1389][drift]
tracked one such anchor into `kawa_h1/editor.rs` across three trees: correct in the first, already
wrong in the second, wrong again in the third, and non-blank at every step.

Two narrower gaps are deliberate. Only the two **ends** of a range are required to be non-blank:
interior blank lines are normal in a span that covers a whole branch — 26 of them across the
guarded surface once the module `LIFECYCLE.md` repair has landed, 57 before it — so requiring every
line would reject correct citations. And a cited path binds
to the citing document's own directory before the repository root, which is what lets a module
`LIFECYCLE.md` write `h2.rs:NNN` for its own sibling; without it the guarded surface reports 251
false ambiguities. The cost is that a sibling could shadow a repo-root file of the same relative
path and hide a real failure. No such pair exists in the tree today, but it is the reason to write
the repo-root-relative path whenever a citation leaves its own module.

The test-name rule is a floor in the same way. A test name of four segments or fewer is not
examined, prose that never says "test" is not examined, and a citation naming a real `fn` that is
not the test the prose means still passes. The alternative is 864 sites of noise, which nobody reads
and therefore nobody maintains.

A green `Doc citations` run means "no citation is obviously dead, and none that this changeset moved
was left behind". It does not mean the citations are right, and it is not a licence to skip reading
the code when you touch one. Where the prose names an item, cite the symbol and the question does
not arise.

One surface gap is known and left open on purpose. The line-citation rules read `doc/**` and every
`**/LIFECYCLE.md`; the test-name rule additionally reads every `*.rs` comment and `CHANGELOG.md`.
Aligning them was measured on `265d895d`: it adds 195 citations across 190 files and **50**
pre-existing failures, 27 of them in `CHANGELOG.md`, which is an append-only record of the tree as
it stood at each release and must not be renumbered to satisfy a guard. That repair is its own
changeset, and it would not reach `e2e/COVERAGE.md` either — no rule reads that file today.

[md-cit]: https://github.com/sozu-proxy/sozu/issues/1444
[cit]: https://github.com/sozu-proxy/sozu/issues/1335
[drift]: https://github.com/sozu-proxy/sozu/issues/1389

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
