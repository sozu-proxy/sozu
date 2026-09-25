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
choice between them is not stylistic. They govern prose inside Rust source too, with one narrowing
— see [Citing code from a Rust comment](#citing-code-from-a-rust-comment):

* **The prose names an item** — a function, method, struct, enum, field, constant or macro — so cite
  the *symbol*, qualified as `Type::method` so it stays greppable, with the file path and no line
  number: `TcpSession::effective_session_address` (`lib/src/tcp.rs`). A symbol survives every
  edit above it.
* **The prose means a specific statement or branch inside an item** — one `match` arm, one guard, one
  log line — so cite a line or a range: `lib/src/tcp.rs:NNN-MMM, NNN-MMM`. Keep the path
  repo-root-relative; a bare `manager.rs` is ambiguous in this tree. A second site in the same file
  may continue from the first without repeating the path — see
  [Continuing a citation without repeating the path](#continuing-a-citation-without-repeating-the-path).
* **The prose QUOTES code in a fenced block** — so put the citation in the fence's info string and
  let the checker compare the quote to its source, modulo leading and trailing whitespace. See
  [Pinning a quoted code block](#pinning-a-quoted-code-block).

A line number carries no anchor. It rots the moment anyone edits the file it points into, and the
pull request that breaks it is almost never the pull request that contains it — so no reviewer is
ever shown both halves. In [sozu-proxy/sozu#1335][cit] an audit of one module document found 29 of
its 35 citations wrong: single lines uniformly off by +1 after a `//!` module-doc block was inserted
above them, and ranges off by +38 to +57 after a `debug_assert!` campaign grew the functions. A range
that drifts 46 lines does not mislead slightly — it lands the reader in a different branch.

### Citing code from a Rust comment

The same three forms govern prose inside Rust source, with one narrowing: **in a `//`, `///` or
`//!` comment, cite the symbol and never a line number.** A comment that means one specific branch
names that branch in words — the `H2WritePhase::Flush` arm of `ConnectionH2::poll_write_target` —
rather than reaching for the second form.

The narrowing is not stylistic either, and it is now enforced: a `path.rs:NNN` written in a `//`,
`///` or `//!` comment **fails CI**. Until [sozu-proxy/sozu#1473][cmt] it failed nothing at all —
the resolver read `doc/**` and every `**/LIFECYCLE.md`, a citation inside a comment was outside its
scope entirely, and the second form there was a line number with no checker behind it while the
pull request that broke it was never the one that contained it. Measured at main `19fd5d8c`, before
this convention reached the source: the tree carried 90
`path.rs:NNN` citation tokens in Rust comments across 25 files, holding 113 line targets between
them. **Sixty-eight of the 90 had drifted** since the commit that last wrote or re-anchored them.
Forty-three of those 68 broke on the very NEXT commit that touched the file they point into; the
median citation survived **one** such commit and zero days, and 65 of 68 were wrong within a week.
A pointer with that half-life is not a maintenance problem to be tightened — it is a form that does
not work. The worked case is the one that reaches furthest: `h2.rs:NNN` written for
`ConnectionH2::write_streams` landed, at that revision, on an unrelated `H2State::ClientPreface`
match arm — resolving cleanly, not blank, and reading like a real place in the file. The tense is
the revision's, not today's: [sozu-proxy/sozu#1479][inv] has since inverted that write path into
`poll_write_target`/`handle_write`, which is the point rather than a caveat — the number moved
again, and the symbols it should have named did not.

Extending the *resolver* over `**/*.rs` was the alternative, and it was measured rather than
argued. Switched on at that revision it would have had to reject those 68 drifted citations, plus
14 naming a basename that is ambiguous in this tree — four files here are called `h2.rs` and
nineteen `mod.rs` — plus two pointing into the `kawa` dependency, which is not in this repository
at all. A gate that fails on most of the population it guards is a gate that gets bypassed rather
than satisfied, which is the argument `check_doc_citations.py`'s own header makes about
`--all-features`. It would also have widened the drift rule across a codebase where line shifts are
constant and legitimate, making a correct edit report a failure.

So what shipped is the cheaper and stricter rule: **forbid the form**, do not resolve it. The
population had already been converted whole, so the rule started green and stays green by conversion
rather than by renumbering. It scans every `*.rs` in the tree, and a cited path binds to the citing
file's own directory first — which is how it reaches a bare `h1.rs:NNN` written from `mux/h2.rs`,
the shape a grep keyed on a `lib/src/`-style prefix walks straight past. A citation naming no file
in this repository is left alone: the two `kawa` ones are a pinned dependency this tree does not
edit, so nothing here can move them. The symbol form costs a reader one `git grep` and cannot drift
at all.

One escape hatch exists, and it is not for a citation that is merely awkward to convert. A comment
that records a position **at a revision that no longer exists** — `h2.rs:NNN` for call sites a later
extraction deleted — cannot be converted to a symbol and must not be renumbered onto today's tree,
because renumbering falsifies a record instead of repairing it. Such a citation is declared in
`HISTORICAL_CITATIONS`, keyed on its citing file and its exact text, with the reason it is there.
An entry without a reason is an unreviewed silencing of the rule, exactly as for `NOT_A_TEST`.

### Continuing a citation without repeating the path

Prose that names two sibling sites in one file reads badly if it spells the path twice, so the
second may be written as a bare span — `` `lib/src/tcp.rs:NNN` and `:MMM` ``. The checker resolves
the bare half by inheriting the path from the citation **earlier on the same source line**, and
then subjects it to every rule the written-out half gets — with one exception, at the end of this
section.

Three constraints, and all three are load-bearing. Every count below is read at `ce80b00c`, the
revision this section was written against; documenting the form adds seven colon-leading code spans
to the guarded surface, so re-measuring the second count at the commit that added this section
reads 55 rather than 48:

* **Backticks.** The span must be a code span of its own. Without that the pattern would also claim
  the 154 bare `:NNN` these documents carry in TOML listen addresses, `curl` URLs, statsd lines and
  log timestamps.
* **A colon, one span group, and nothing else inside the span.** A single line, a range and a
  `,`/`/` group all count, exactly as in a written-out citation — the bare form is a continuation of
  that grammar, not a narrower one. What it excludes is everything that is not a line number: 48
  code spans here begin with a colon and 45 are HTTP/2 pseudo-headers or ordinary prose, most of
  them `` `:authority` `` and `` `:status` ``. `doc/testing.md` writes `` `:status` `` three times,
  once in the very paragraph that carries a continuation of its own.
* **The same line.** These sites sit inside bullet lists that run 15, 16 and 101 lines without a
  blank, so a paragraph-wide carry would bind a stray port number to a path named a hundred lines
  earlier and report it as resolved. If a reflow moves the bare half onto the next line the checker
  reports it rather than falling silent, and you write the path out.

Until [sozu-proxy/sozu#1459][bare] the resolver did not see this form at all, for the same reason it
did not see a markdown target until #1444: the pattern required a path, so a citation carrying none
was never extracted, never resolved, never drift-checked, and never reported as unresolvable either.
The first half of every pair was checked on every CI run and the second half had never been checked
once — and the two halves name sibling sites in the same function, so they rot together.
[sozu-proxy/sozu#1465][bare-eg] is the worked case, and it is why this tree carried a wrong citation
rather than three correct ones: it moved both call sites of a test helper down one line, bumped the
written-out half of the pair in `doc/testing.md`, and left the bare half behind. Both numbers were
right before it and only one was right after, with the `Doc citations` job green throughout, because
the only rule that could have seen the bare half never extracted it. Three sites existed when the
form became visible and that one was wrong — it named the line above the call site the prose is
about.

One rule does not reach the bare form, and it is the one nearest to it. A written-out `/` or `,`
group is rejected when the same line repeats inside it, which is what a continuation renumbered on
one half only looks like; a bare **group** is checked the same way, but a bare span written as its
own separate code span is a separate citation, so a path cited once and then continued bare at the
same line number is not caught. [sozu-proxy/sozu#1457][ident] closed the drift half of that
identity problem — the drift rule keys its exemption per span, below — but this is rule 1's half, a
duplicate across two citation groups, and it is still open.

(This section is spelt without a real line number for the reason the one below it is: this document
is inside the guarded surface, so a worked example written out in full would be resolved as a
citation, and a bare one used as an illustration would be reported as a citation with no path.)

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
them — bare `:NNN` continuations included, under the path they inherit from the same line — and
fails when a cited file does not exist, a cited basename is ambiguous, a line number is
below 1 or past end-of-file, either end of a range is blank, a range is inverted, a bare
continuation has no citation earlier on its own line to inherit from, or the same line
repeats inside one citation group — `file.rs:NNN/NNN`, which is what a `/` or `,` continuation
renumbered on one half only looks like, and which would otherwise resolve perfectly.

The same run additionally **forbids** a `file.rs:NNN` written inside a `//`, `///` or `//!` comment
in any `*.rs`. That rule resolves nothing and renumbers nothing: it reports the citing site, the
citation text and the file it names, and asks for the symbol instead. A citation naming no file in
this repository is counted and skipped, and both counts are printed beside each other so a green
run cannot hide how much it declined to look at.

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
span this changeset **re-anchored** is not reported: only one that carries the same path and the
same line span it carried at the base, whose claim this changeset did not carry to another number,
while the text underneath it changed. Comparison is on the stripped line, so a re-indent is not
drift.

The exemption is keyed **per span, not per citation**. Keyed on the whole group, correcting one
number in a four-span `file.rs:NNN/MMM/PPP/QQQ` changed the identity of the entire citation and
bought every sibling an exemption, so the author who half-applied a renumbering hid the rest — and
the more careful the partial fix looked, the more numbers it hid. A group where element 1 is freshly
correct reads as maintained, which is worse than one that is uniformly stale. That was
[sozu-proxy/sozu#1457][ident]. `06fc2708` is the worked case: it moved element 1 of three groups
in the mux `LIFECYCLE.md` by the correct shift, left seven sibling numbers on unrelated code, and
the job was green. The per-span key reports six of the seven. The seventh escapes it too, because
the text at the line it names happens to equal the text at the line it used to name — a comparison
of text cannot see a number that moved between two identical lines.

Both ends of a **range** stay one span, so correcting only the end of a `file.rs:NNN-MMM` re-anchors
its start as well. The two ends of a range describe one construct and move together, and keying per
end instead would compare a fresh single `file.rs:MMM` against the end of an older
`file.rs:NNN-MMM` — a new false positive for no coverage.

The `Doc citations` CI job passes the pull request's base sha, and checks out with `fetch-depth: 0`
because the default shallow checkout has no base commit to read. An unreachable `--base` is an
**error and exit 1**, never a skip — a guard that answered a missing base with a clean run would be
green forever while comparing nothing. With no `--base` at all, the run says on its own line that
the rule did not run.

It remains a floor in one direction: a citation into code that this changeset never touched is not
compared. Cite a symbol wherever the prose names an item.

**The identity is the cited line, not its number.** A number is not stable under the edits the rule
exists to police, so an exemption keyed on one answers wrong in a way no edit to the document can
repair: renumbering a citation onto a line whose number the base revision spent on something else
leaves the span not-new, and a *correct* citation is reported as drift. [sozu-proxy/sozu#1447][reuse]
found it in the H2 stack: at `595920e9` the mux `LIFECYCLE.md` cited one `h2.rs` line for the
`handle_goaway_frame` retry loop, and on a branch that renumbered after a large `h2.rs` edit the
`StreamState::Link` transition landed on that very line, so repointing the Link citation at its true
new line was reported as drift with no edit available that would silence it.

So the rule tracks the **text** a citation named at the base and watches it move. A span is declined
when the line it named at the base revision is cited again at a number this changeset *introduced* —
the base document's claim is alive somewhere else, so the span under test is a different citation
that inherited the number rather than the base one left behind. Nothing guesses: a receiver is only
a number this document did not cite at the base at all, and the receivers for a line must be at
least as many as the base citations that named it, so a document that named one line twice and
re-anchored one of the two still has the other reported. That second condition is what keeps
[#1457][ident]'s half-applied renumbering visible. Measured on a branch that re-anchors 72 spans:
the number-keyed rule reported one mux `h2.rs` citation as drift because the base revision had spent
that number on `context.unlink_stream(stream_id);`, while that very line is cited — correctly, and
by the same changeset — 39 lines earlier. The text-keyed rule declines the span and names both.

Two residuals stay, and both are **printed rather than silent**. A base citation this changeset
deleted instead of re-anchoring leaves no receiver, so a correct citation landing on the deleted
one's line is still reported. And a cited line carrying little text — a lone `}`, a bare `where` —
can be matched by coincidence, in which case a real drift is declined. Every declined span is listed
with the text that was matched and the number the claim moved to, so the evidence is on screen and a
coincidence reads as one; `--show` tags them `|reused-number`. A declined span is a comparison the
rule did **not** perform, so it lowers the compared total rather than hiding inside it. Note what
this does not say: declining a span says the base's claim moved, never that the number the changeset
wrote is a sensible place to point at. That question is `--audit`'s, and a symbol's — a symbol has no
number to reuse.

**Every run prints how many spans it did not look at.** A compared total reports what the rule
examined and never what it declined to examine, so its coverage can fall to zero for a whole
changeset with every counter it emits still healthy. Measured in #1447 on a changeset that repaired
six markdown citations: 370 cited ends compared with `.md` targets on, 370 with them off, and *zero*
of the six entered the rule — repairing a citation changes its span, which is exactly what the
exemption covers. The run said "none of the 370 cited lines changed their text" and was telling the
truth about the 370 while saying nothing about the six. `compared` and `exempt` now partition every
cited end that resolved and was in range at HEAD — together with the spans declined as a reused
number above — and `--show` lists each exempt span as `|re-anchored`. "38 compared, 12 exempt" says
there is something to disposition where "38 compared" reads as complete coverage. A span renumbered into the **grown tail** of a file — past the end the
base revision had — counts as exempt like any other, which matters because an insertion of any size
pushes later citations past the old end by construction; binding the exempt count to the base range
would have dropped exactly the changesets that re-anchor the most.

`--self-test` is what keeps the checker honest. It asserts the exact failures a deliberately broken
fixture must produce, *and* runs the real command line in a subprocess to require exit `1` on that
fixture and exit `0` on a clean one — because reporting a failure and acting on it are two different
lines of code, and a checker that did the first and not the second would be green forever. The drift
half is asserted the same way and in both directions: `testdata/citations/` carries a document whose
every citation resolves to a non-blank line at **both** revisions, so the same tree must exit `0`
without `--base` and `1` with it, and an unreachable base must be refused rather than skipped.

### Auditing citations that never change

Everything above, the drift rule included, is a *change* detector, and a change detector cannot find
a defect that predates its first observation. Rule 2 compares a citation's text between two
revisions, so a citation that was already wrong the first time it was seen is exempt **forever** —
"unchanged" is exactly the condition for being exempt. [sozu-proxy/sozu#1466][audit] measured four
of them in the mux `LIFECYCLE.md`, byte-identical across the whole series and wrong in every one:
three landed on comment lines under prose naming an insert or a push, and the fourth cited
`self.stream_table.rst_sent_contains(sid)` against a line reading `let total_before = *total;`.
[sozu-proxy/sozu#1493][comment] closed that hole for Rust comments by forbidding the form outright,
which needs no heuristic. These documents keep their line numbers, because here a number is
sometimes the only way to name a span with no symbol — so this surface gets an audit instead.

```
python3 .github/scripts/check_doc_citations.py --audit --root .
```

It reads every citation's *prose* against its cited *line* and reports what does not plainly match,
on two heuristics. **The target is a comment** while the citing prose names a statement, a call, an
insert or a push — single-line citations only, because a range that starts on a comment is the
normal way to cover a branch together with the sentence introducing it, and reading ranges too
doubles the output with citations that are all correct. **The prose names a symbol that is not
there** — neither within a few lines of the cited span nor as an item enclosing it — which is what
reaches a wrong citation that lands on ordinary code and looks healthy to everything else.

**It is advisory and it is not in CI.** It always exits `0`, it never edits a citation, and the
`Doc citations` job does not run it. A heuristic that fails a build is a heuristic people learn to
silence, and the silencing outlives the reason. What it produces is a list to disposition — `wrong`,
`correct`, or `false positive` **with the reason** — and the disposition is the deliverable. A false
positive is information about the heuristic, not noise: either the heuristic is wrong about a shape
and should be narrowed, or it cannot tell and the limitation gets written down.

**Expect roughly half of it to be wrong, and do not tune that away.** Its own first run examined 108
of the guarded surface's 221 cited spans and reported 13; hand-audited, six were wrong citations and
seven were false positives. A mode that found nothing on a tree known to contain wrong citations
would be worse than no mode, so the ratio is printed in its own output rather than engineered down.
The six repaired: two in `doc/testing.md` pointing at a comment line rather than the check or the
byte it describes, one in the `kawa_h1` `LIFECYCLE.md` pointing at a bare `}` where the function it
names is fifty lines below (repaired by dropping the number — the prose already names the symbol),
and three in the mux `LIFECYCLE.md` naming `Mux::shutting_down` while pointing into
`Mux::shutting_down_inner`, the body that the four-line `SessionState` wrapper drives.

**Every run prints what it declined to check.** The largest class is prose that names nothing the
heuristics can test — 111 citations on that same run, more than half the surface — a sentence
carrying no backticked symbol and none of the statement nouns, inside which a wrong citation is
invisible here. A prose-to-prose citation is declined too, having no code shape to read, as is any
span rule 1 already rejects. Silence about skipped work is the defect behind
[sozu-proxy/sozu#1457][ident] and [sozu-proxy/sozu#1447][reuse], so the counts print whether or not
anything was found.

And it misses things while looking straight at them. The mux `LIFECYCLE.md`'s `Mux::timeout`
citation carried the same defect as the three `shutting_down` ones above — its number pointed into
`Mux::timeout_inner` — and the audit did not report it, because the word `timeout` appears in a
`trace!` and a comment near the cited line. It was found by hand while dispositioning the finding
on the bullet directly below it, and repaired with them. Proximity cannot tell "the name is nearby"
from "the item is here". Treat a clean audit as "nothing
obvious", never as "the citations are right"; the remedy that actually ends the class is to cite a
**symbol**, which has no number to audit.

**Widening its prose window was measured and rejected.** [sozu-proxy/sozu#1531][widen] filed a
citation in `doc/lifetime_of_a_session.md` naming `accept_queue.saturated_seconds` while pointing at
a buffer-pool gauge block fifteen lines above the ticker, and asked whether this mode could reach
it. The signal it needs already exists — *the prose names a symbol that is not there* — but the
citation sits mid-line under the sentence naming the metric, so the fragment read is its own line
alone, and both of that document's spans were *declined* rather than examined. Reading one line
further up takes the mode from 7 findings to 13, of which one addition is a real defect; taking
every symbol the surrounding prose names, which is the literal rule that issue proposed, takes it to
17, of which two are real and three correct citations it reports today go quiet. Against the six in
thirteen this mode measured for itself and prints in its own banner, one in six is the rate at which
a reviewer stops reading — so the item was closed rather than shipped.

Worse, the variants that do fire on it fire by a one-line margin. The wrong span ends seven lines
above the ticker's own comment, and that comment names the metric, so a window of eight excludes it
by exactly one line; at nine every variant goes quiet while the citation stays just as wrong. The
correct companion span in the same sentence is silent for the same reason in reverse — it opens on
the constant's doc comment, which names the metric. Same mechanism, opposite verdicts, one line
apart. What repaired that document was the remedy above: its five misaligned spans became symbols,
and the declined count fell from 95 to 90, because every one of them had been in the bucket this
mode cannot read.

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
```rust lib/src/protocol/mux/hpack_state.rs:128-138
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
(`lib/src/protocol/mux/hpack_state.rs:128-138, 146`), and the block must then be their concatenation
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
the drift rule accepts everywhere else and it is not closed here. It is no longer *silent*, though:
that run would now print the exempt count beside the compared one, which is the number that said
nothing while 325 held steady.

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

One surface gap is known and left open on purpose. The line-citation rules RESOLVE citations in
`doc/**` and every `**/LIFECYCLE.md`; the test-name rule additionally reads every `*.rs` comment and
`CHANGELOG.md`. Aligning the resolver with them was measured on `265d895d`: it adds 195 citations
across 190 files and **50** pre-existing failures, 27 of them in `CHANGELOG.md`, which is an
append-only record of the tree as it stood at each release and must not be renumbered to satisfy a
guard. That repair is its own changeset, and it would not reach `e2e/COVERAGE.md` either — no rule
reads that file today.

The comment rule does not close that gap and does not try to. It reads every `*.rs` and **forbids**
rather than resolves, so it imports none of those 50: it asks for no number to be correct, only for
no number to be there. `CHANGELOG.md` stays outside it for the reason above — a quoted tool
transcript in a release entry is a record of what a command printed on the day it ran, and a rule
that made it fail would be asking for the record to be rewritten. `e2e/COVERAGE.md` stays outside
every rule here, unchanged.

[cmt]: https://github.com/sozu-proxy/sozu/issues/1473
[inv]: https://github.com/sozu-proxy/sozu/pull/1479
[md-cit]: https://github.com/sozu-proxy/sozu/issues/1444
[bare]: https://github.com/sozu-proxy/sozu/issues/1459
[bare-eg]: https://github.com/sozu-proxy/sozu/pull/1465
[ident]: https://github.com/sozu-proxy/sozu/issues/1457
[reuse]: https://github.com/sozu-proxy/sozu/issues/1447
[cit]: https://github.com/sozu-proxy/sozu/issues/1335
[drift]: https://github.com/sozu-proxy/sozu/issues/1389
[audit]: https://github.com/sozu-proxy/sozu/issues/1466
[widen]: https://github.com/sozu-proxy/sozu/issues/1531
[comment]: https://github.com/sozu-proxy/sozu/pull/1493

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
