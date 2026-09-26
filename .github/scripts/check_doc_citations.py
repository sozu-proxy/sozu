#!/usr/bin/env python3
# Resolve every citation in this repository's prose against the tree it ships
# with, and fail when one of them cannot be resolved. Three citation forms are
# checked, by five independent rules — four that RESOLVE a citation, and one
# that FORBIDS a form outright:
#
#   1. `file.rs:NNN(-MMM)?` and `file.md:NNN(-MMM)?` in `doc/**` and
#      `**/LIFECYCLE.md` — a path and a line number — plus the bare `` `:NNN` ``
#      CONTINUATION of one, which names no path and inherits the one from the
#      citation earlier on its own line. See "WHAT THIS CATCHES" below,
#      "MARKDOWN TARGETS" for why a `.md` target is here, and "BARE
#      CONTINUATIONS" for the form that no rule could see at all until
#      sozu-proxy/sozu#1459.
#   2. the same citations, read at TWO revisions: a citation this changeset did
#      not touch must still name the same line TEXT it named at the base. What
#      counts as "did not touch" is the cited LINE and not its number — see
#      "DRIFTED CITATIONS" and "A NUMBER REUSED FOR DIFFERENT CODE" further
#      down. Needs `--base <revision>`.
#   3. a backticked TEST NAME, in a Rust comment or a CHANGELOG/doc paragraph,
#      that names no `fn` anywhere in the tree. See "DEAD TEST-NAME CITATIONS"
#      further down.
#   4. a fenced code block whose INFO STRING names the lines it quotes —
#      ```rust path/to/file.rs:NNN-MMM — must quote them literally. See "STALE
#      CODE QUOTED IN PROSE" further down.
#   5. a `file.rs:NNN` citation written inside a `//`, `///` or `//!` COMMENT,
#      in any `*.rs` in the tree — forbidden outright rather than resolved.
#      This is the one rule here that does not ask whether a number is right;
#      it asks that there be no number. See "LINE NUMBERS CITED FROM A RUST
#      COMMENT" further down.
#
# Plus one MODE that is not a rule: `--audit` reads a citation's prose against
# its cited line and reports what does not plainly match. It resolves nothing,
# forbids nothing and always exits 0, because its answer is a heuristic. See
# "AUDITING A CITATION THAT NEVER CHANGED" below for the two signals, the
# measured false-positive rate, and why it is not in `ci.yml`.
#
# Why this exists: a line number carries no anchor, so a citation rots the
# moment anyone edits the file it points into — and the edit usually lands in a
# *different* pull request than the one holding the citation, so no reviewer is
# ever shown both halves. See sozu-proxy/sozu#1335.
#
# WHAT THIS CATCHES
#   - the cited file does not exist, or its basename is ambiguous in the tree
#   - the cited line is past end-of-file
#   - the cited line is blank
#   - a range whose start is greater than its end
#
#   A cited path is resolved against the document's own directory first (a
#   module LIFECYCLE.md cites its siblings by bare name), then the repository
#   root, then a unique tree-wide suffix match.
#   - the same line repeated inside one citation group (`answers.rs:224/224`),
#     which is the signature of a half-applied renumbering: the `/` continuation
#     form is real in this tree (`answers.rs:209/224`, `mod.rs:1218/1233/1311/1321`)
#     and a rewrite that touches only its first half produces a duplicate that
#     resolves perfectly and is therefore invisible to every other check here
#   - a bare `` `:NNN` `` continuation with no citation earlier on its own line
#     to inherit a path from, which is reported rather than skipped: see "BARE
#     CONTINUATIONS"
#
# WHAT RULE 1 ALONE DOES NOT CATCH — READ THIS BEFORE TRUSTING A GREEN RUN
#   Rule 1 is a floor, not a proof. It cannot tell whether a citation lands on
#   *the construct the surrounding prose is talking about*. A citation that
#   drifted from line 118 to line 164 still resolves, still hits code, and
#   still passes rule 1 while pointing the reader at a different branch. That
#   is the dominant failure mode, not the exotic one. Measured on the module
#   LIFECYCLE.md files at main `ba7fa5f9`: this resolver flagged 27 of the 507
#   anchors those documents carry, while the hand audit that followed cut them
#   to 238 and had to renumber 159 of the survivors. Rule 2 closes that gap for
#   any line the changeset under test actually moved, which is where a citation
#   rots; nothing mechanical closes it for a line nobody touched.
#
#   Two narrower gaps, both deliberate. Only the two ENDS of a range are
#   required to be non-blank: interior blank lines are normal in a span that
#   covers a whole branch (26 of them across the guarded surface today), so
#   requiring every line would reject correct citations. And because a cited
#   path binds to the document's own directory first, a sibling can shadow a
#   repo-root file of the same relative path and hide a genuine failure. That
#   rule removes 251 false ambiguity reports and there are no shadowing pairs
#   in the tree today, so it pays for itself — but write the repo-root-relative
#   path when a citation leaves its own module.
#
#   A THIRD GAP, not deliberate, just unguarded: every rule here resolves
#   SOURCE targets only. A prose-to-prose citation — `configure.md:933` — is
#   matched by no rule, so it is never resolved, never drift-checked, and a
#   changeset that inserts lines into the cited document rots it invisibly.
#   That is not hypothetical: the changeset that added this note shifted
#   `doc/configure.md` by 46 lines under `doc/configure_admin_ops.md:230`,
#   with this script green throughout. Note what the stale citation pointed
#   AT — not a blank line and not a missing one, but line 933's ordinary
#   prose about flow keys, and at an intermediate revision a line holding a
#   lone `]`. Both resolve, both read as a real place in the document, and
#   neither is what the citing sentence is about. That is the whole failure
#   mode: nothing about a rotted prose citation looks wrong.
#
#   There are exactly three such citations in the tree
#   (`doc/configure_admin_ops.md:79`, `:157` and `:230`, all into
#   `doc/configure.md`), which is too few to justify a fourth rule — so it is
#   a hand check: edit a document that any of those three cites, and re-point
#   them.
#
#   The remedy for the rest is to cite a *symbol*
#   (`ExpectProxyProtocol::readable`) wherever the prose names an item, and to
#   keep a line or a range only where the prose means a specific branch or
#   statement inside an item. A symbol cannot drift; a line always can. Treat a
#   green run as "no citation is obviously dead, and none that this changeset
#   moved was left behind", never as "the citations are right".
#
# DRIFTED CITATIONS
#   Rule 1 fires only when a citation lands on a BLANK line, which is a small
#   corner of the way citations rot. sozu-proxy/sozu#1389 measured the rest:
#   on sozu-proxy/sozu#1379's changeset, which added +21 lines to
#   `kawa_h1/editor.rs` and +14 to `mux/router.rs`, 24 citations moved and rule
#   1 reported 2. The 22 it could not see all landed on non-blank code —
#   `kawa_h1/LIFECYCLE.md` alone cites `editor.rs` 21 times. Worse, the guard's
#   presence was itself the hazard: a green `Doc citations` job reads as "the
#   citations are right" while it only ever meant "no citation landed on a
#   blank line".
#
#   Rule 2 closes that with the base revision and nothing else. Every citation
#   is resolved with rule 1's own `resolve_path`/`CITATION` — a naive basename
#   match gives false positives, because a bare `mod.rs` in `mux/LIFECYCLE.md`
#   binds to its sibling — and the TEXT of the cited line is read at the merge
#   base and at HEAD. Different text is reported, blank or not, which makes
#   this a strict superset of rule 1 for every line the changeset touched.
#
#   An author who RE-ANCHORS a citation is not drifting it, so a SPAN is
#   compared only when the identical `path` and that same line span are also
#   present in the BASE revision of its own document — and only when the base
#   document's claim on that number has not demonstrably moved somewhere else,
#   which is the second half of the identity and is in "A NUMBER REUSED FOR
#   DIFFERENT CODE" below. Identity is the citation, not its position in the
#   file: moving a paragraph does not excuse a stale number, and repointing
#   `editor.rs:1131` at `editor.rs:1152` is silently accepted. Comparison is on
#   the STRIPPED line, so a pure re-indent is not drift.
#
#   PER SPAN, not per group — sozu-proxy/sozu#1457. Keyed on the whole tuple,
#   correcting ONE number in `h2.rs:5676/5860/5885/5903` changed the identity
#   of the entire citation, so all four spans were classified "re-anchored"
#   and the three stale ones were never compared. That is the half-applied
#   renumbering this file names in its own WHAT THIS CATCHES list, and the
#   whole-tuple key handed it an exemption: the author who fixed one number
#   bought silence for the rest, and the more careful the partial fix looked,
#   the more numbers it hid. It is worse than leaving the group alone — an
#   all-stale group reads as uniformly suspect, while a group whose first
#   element is freshly correct reads as maintained. Measured on `06fc2708`
#   ("refactor(mux): hand the Endpoint trait an RTT value, not a peer
#   socket"), which moved element 1 of three groups in
#   `lib/src/protocol/mux/LIFECYCLE.md` by the correct shift and left seven
#   sibling numbers on unrelated code with this job green: the per-span key
#   reports six of the seven.
#
#   The seventh, `mod.rs:2152`, escapes the per-span key too, because the text
#   at that line happens to equal the text at the line it used to name. A
#   comparison of TEXT cannot see a number that moved between two identical
#   lines, and nothing short of resolving the enclosing construct could — the
#   text-keyed identity below does not reach it either, for the same reason.
#
#   One residual inside a RANGE: `8-13` is one span, so fixing only its end to
#   `8-14` re-anchors the start along with it. Both ends of a range describe
#   one construct and move together far more often than two group elements do,
#   and splitting the key per END would compare a fresh single `x.rs:13`
#   against an old `x.rs:8-13`'s end — a new false positive for no coverage.
#   Left as it is, deliberately.
#
# A NUMBER REUSED FOR DIFFERENT CODE — KEYED ON THE TEXT, NOT ON THE NUMBER
#   A NUMBER is not stable under the edits this rule exists to police, so an
#   exemption keyed on one answers wrong in a way no edit to the document can
#   repair. When a changeset renumbers a citation onto a line whose number the
#   base revision spent on something else, the span is not new, a number-keyed
#   exemption does not apply, and a CORRECT citation is reported as drift.
#   sozu-proxy/sozu#1447 found it in the H2 sans-io stack: at merge base
#   `595920e9`, `lib/src/protocol/mux/LIFECYCLE.md:469` cited `h2.rs:6248` for
#   the `handle_goaway_frame` retry loop; on a branch that renumbered citations
#   after a large `h2.rs` edit, the `StreamState::Link` transition landed ON
#   line 6248, and renumbering the Link citation to its true new line was
#   reported as drift.
#
#   This is not rare arithmetic. `doc/h2_mux_internals.md` alone carries 18
#   pinned blocks into `h2.rs`, one branch renumbered 75 citations in a single
#   pass, and number reuse across a document with ~100 citations into an
#   8800-line file is a coincidence you buy once per citation.
#
#   THE IDENTITY IS THE CITED LINE, AND THE RULE CAN SEE IT MOVE. `check_drift`
#   asks `reanchored_claims` for every cited line TEXT whose claim this
#   changeset carried onto a new number, and declines a span whose base text is
#   one of them: what the base document named at that number is alive
#   elsewhere, so the span under test is a different citation that inherited
#   the number rather than the base one left behind. Measured on
#   `refactor/h2-goaway-drain` against `921c13673616`, which re-anchors 72
#   spans: the number-keyed rule reported `h2.rs:5994` as drift because the
#   base document had spent 5994 on `context.unlink_stream(stream_id);`, while
#   that very line is cited — correctly, and by this changeset — at
#   `h2.rs:5955`. The text-keyed rule declines the span and names both.
#
#   NOTHING HERE GUESSES, and two counted conditions are what keep it that way.
#   A RECEIVER is only a head-cited end whose number this document did not cite
#   at the base at all, because a number the base already carried explains
#   nothing — it is as likely to be a second stale citation, which is what two
#   adjacent citations do when their file loses a line and each lands on its
#   neighbour's old text. And the receivers for a text must be AT LEAST AS MANY
#   as the base citations that named it: a document that named one line twice
#   and re-anchored one of the two has left the other behind, which is
#   sozu-proxy/sozu#1457's half-applied renumbering and must keep being
#   reported. `doc/drift.md` is that fixture — its base cites `drift.rs:12`
#   twice and this changeset re-anchors one to `:16` — so widening the
#   exemption past its counting turns the self-test red instead of quietly
#   turning the rule off.
#
#   TWO RESIDUALS, PRINTED RATHER THAN SILENT. A base citation this changeset
#   DELETED instead of re-anchoring leaves no receiver, so a correct citation
#   landing on the deleted one's line is still reported as drift, exactly as
#   before. And a cited line carrying little text — a lone `}`, a bare `where`
#   — can be matched by coincidence, in which case a real drift is declined.
#   Neither is silent: every declined span is printed with the text that was
#   matched and the number the claim moved to, so the evidence is on the
#   reviewer's screen and a coincidence reads as one. A declined span is a
#   comparison this rule did NOT perform, which is why it lowers the compared
#   total and is listed rather than folded into the re-anchored count.
#
#   This rule also cannot say whether the number a changeset WROTE is a
#   sensible place to point at — declining `h2.rs:5994` above says the base's
#   claim moved, not that 5994 is a good citation. That question belongs to
#   `--audit`, and to citing a SYMBOL wherever the prose names an item, which
#   is the remedy the rest of this file keeps naming. A symbol has no number to
#   reuse.
#
#   AND THE EXEMPTION IS COUNTED. A compared total reports what the rule
#   looked at and never what it declined to look at, so this rule's coverage
#   could fall to zero for a whole changeset with every counter it emitted
#   still healthy. #1447 measured that too: on a changeset repairing six
#   markdown citations, 370 cited ends compared with `.md` targets enabled and
#   370 with them disabled — zero of the six entered the rule, because
#   repairing a citation changes its span, which is exactly what the exemption
#   covers. The run said "none of the 370 cited lines changed their text" and
#   was telling the truth about the 370 while saying nothing about the six.
#   Every run now prints the exempt count beside the compared one, and
#   `--show` lists each exempt span as `|re-anchored`. It is the cheapest
#   mitigation for the reuse case above as well: a reviewer who reads
#   "38 compared, 12 exempt" knows there is something to disposition.
#
#   It FAILS CLOSED. The base revision has to be in the object store, and the
#   default `actions/checkout` is shallow, so `git merge-base` exits non-zero
#   there. Treating that as "nothing to compare" would report a clean run while
#   checking nothing — the exact shape this rule exists to close — so an
#   unreachable `--base` is an error and exit 1, never a skip. Without `--base`
#   at all the rule announces that it did not run, on its own line.
#
# The regex is deliberately `[A-Za-z0-9_/.-]+\.(?:rs|md):[0-9]+`. The obvious
# character class `[A-Za-z_/.-]+` has no digit in it and silently skips every
# citation naming a file with a digit in its name — `h1.rs:NNN`, `h2.rs:NNN`,
# which in this tree is the majority of them.
#
# MARKDOWN TARGETS
#   A cited path may be a `.md` as well as a `.rs`, and it is resolved by all
#   three rules above, with no exemption of its own. Prose cites prose: an
#   operations runbook points at the reference table that defines the knob it
#   is telling the operator to turn, exactly as it points at the function that
#   implements it. A line number into a document rots the same way a line
#   number into a module does — faster, if anything, since a document grows by
#   whole sections.
#
#   That half of the extraction was missing until sozu-proxy/sozu#1444. `PATH`
#   named `.rs` alone, so `CITATION` walked straight past a markdown target:
#   the citation was never extracted at all, and no rule below it — not the
#   blank-line floor, not the drift comparison — was ever handed one. The
#   surface it left unguarded was not hypothetical: at main `95dee167` the tree
#   carried three markdown-target citation sites holding six line targets
#   between them, all three in `doc/configure_admin_ops.md` and all six
#   pointing into `doc/configure.md`, and SIX OF SIX pointed at unrelated
#   prose — a cipher-suite table row, an `sni_preread_timeout` TOML block, a
#   sentence about gRPC backends. The class the resolver could see was
#   accurate; the class it could not see was wrong in every instance.
#
#   The drift rule was blind to them for the same reason, and that is how the
#   worst of the six got there. sozu-proxy/sozu#1437 re-anchored
#   `configure.md:933` to `configure.md:979` when it shifted lines in
#   `configure.md`, which is mechanically correct — line 933 at `7a223a8f` held
#   the text line 979 holds at `95dee167` — and carried an already-wrong target
#   forward intact, and more precisely than before. A re-anchor preserves the
#   pointer, not the claim, and nothing compares the two.
#
#   This stays a floor for markdown exactly as it is for Rust: a citation onto
#   a real, non-blank, unrelated line resolves and passes, which is what the
#   `--show` output and the summary lines are for. What it can no longer do is
#   name nothing at all and be counted as clean.
#
# BARE CONTINUATIONS
#   `CITATION` requires a path, so the idiom that names a file once and then
#   continues with a bare line number — `` `lib/src/tcp.rs:183` and `:305` `` —
#   put its SECOND half outside every rule in this file. Not rule 1, not rule
#   2, not rule 3, not rule 4: the bare span was never extracted, so it was
#   never even reported as unresolvable. The first half was checked on every CI
#   run and the second half had never been checked once (sozu-proxy/sozu#1459).
#
#   That is worse than an unchecked citation. The two halves name sibling sites
#   in the same function, so they rot TOGETHER, and a reader who watches the
#   guarded half land correctly infers the unguarded half is as maintained.
#
#   sozu-proxy/sozu#1465 is the worked case, and it is why this tree carried a
#   wrong citation rather than three correct ones. `3e9c271c` moved both call
#   sites of `h2_handshake_chromium_146` down one line; it bumped the half a
#   rule could see and left the half no rule could:
#
#       -  `h2_correctness_tests.rs:3604` and `:3708`. No test that decodes …
#       +  `h2_correctness_tests.rs:3605` and `:3708`. No test that decodes …
#
#   Both numbers were right at `3e9c271c^` and only the first was right after.
#   Run THIS file over that changeset and it reports the other one:
#
#       $ git worktree add --detach /tmp/repro 3e9c271c
#       $ python3 check_doc_citations.py --root /tmp/repro --base 3e9c271c^
#       ::error::1 of 215 compared citations drifted since 970b92c5678a:
#         doc/testing.md:680: `h2_correctness_tests.rs:3708` — …:3708 moved:
#         was `let _server_settings = h2_handshake_chromium_146(&mut tls);`,
#         now `let mut tls = raw_h2_connection(front_addr);`
#
#   The `Doc citations` job was green on `3e9c271c` as it shipped, because the
#   only rule that could have seen `:3708` never extracted it.
#
#   So the tree carried three continuations and the class was one in three
#   wrong. `doc/testing.md`'s `:3709` was written `:3708`, which is
#   `let mut tls = raw_h2_connection(front_addr);` — the line above the call
#   site the prose means. `grep -n` puts the two sites at 3605 and 3709, and
#   the citation was re-derived from that grep rather than by offsetting the
#   stale number. The other two are exact: `lib/src/tcp.rs:183`/`:305` are that
#   file's only two `let frontend_address = socket.peer_addr().ok();` lines,
#   and `router.rs:394`/`:574` its only two `context.link_stream(` calls.
#
#   A bare `:NNN` is ORDINARY PROSE, so the extraction is deliberately narrow
#   and both discriminators were measured on this tree rather than guessed.
#   Every count below is read at `ce80b00c`, the revision this changeset
#   departed from. Documenting the form is itself an edit to the guarded
#   surface: this changeset's own prose adds seven colon-leading code spans,
#   so the SECOND count re-measured at the commit that carries this comment
#   reads 55 rather than 48. The other two are unaffected by prose, and the
#   bare-span total stays 3 either way.
#     * THE WHOLE CODE SPAN, backticks included. The guarded surface holds 154
#       bare `:NNN` that are neither part of a written-out citation nor a code
#       span of their own, and not one of them is a citation: TOML listen
#       addresses (`0.0.0.0:8443`, `[::1]:8080`), `curl` URLs, statsd lines
#       (`sozu.WRK-00.http.requests:1|c`), log timestamps (`14:01:51Z`), a
#       `1:100` ratio, `--ulimit nofile=262144:262144`. Requiring the span to
#       be a whole `` `…` `` drops all 154.
#     * A COLON, ONE SPAN GROUP, AND NOTHING ELSE inside it. A range and a
#       `,`/`/` group both count, matching `CITATION`. 48 code spans in the
#       guarded surface begin with a colon and 45 are HTTP/2 pseudo-headers
#       and prose
#       (`:status`, `:method`, `:authority`, `:scheme`, `:path`, `:reason`,
#       `::reclaim`, `:status 404`). Requiring a colon plus one line span and
#       nothing else leaves exactly the three real ones — and note
#       `doc/testing.md` writes `` `:status` `` one sentence after its own
#       continuation, so this is not a hypothetical collision.
#   Together they take the tree from 232 extracted citations to 235: the three
#   that exist, and no others.
#
#   SCOPE. The path is inherited from the nearest written-out citation EARLIER
#   ON THE SAME LINE, which is the tightest scope that covers all three sites —
#   each is written `` `path:N` and `:M` `` on one source line. Wider is not
#   free: all three sit inside bullet lists that run 15, 16 and 101 lines
#   without a blank, so a paragraph-scoped carry would search 101 lines back in
#   `doc/testing.md` and bind a stray `:443` to whatever path it found there. A
#   WRONG binding is worse than none, because it reads like a resolved citation
#   and rule 1 will happily confirm it lands on a non-blank line.
#
#   And a bare span that finds nothing on its line is REPORTED, never skipped.
#   That is what keeps the narrow scope honest: without it, a prose reflow that
#   moved `:305` onto the next line would silently restore the exact hole this
#   section closes, and silence is the whole defect. The author is asked to
#   write the path, which is always available. No site in the tree takes that
#   branch today.
#
#   Each bare span is its own citation carrying the inherited path, so rule 1
#   resolves, range-checks and blank-checks it and rule 2 drift-compares it,
#   exactly as for a written-out one — rule 2 reads the BASE revision through
#   the same `citations()`, so `(router.rs, ((574, 574),))` is an identity at
#   both revisions and #1465's shape — the written-out half re-anchored, the
#   bare half left behind — is reported instead of invisible. On the
#   changeset that first makes one VISIBLE, rule 2 is vacuous rather than
#   absent: the identity is present at both revisions and compares equal,
#   because nothing here edits the files they cite.
#
#   What this does NOT catch. A bare GROUP goes through the repeated-line check
#   like any other group, so `` `:3/3` `` is reported — but `x.rs:3` followed by
#   a SEPARATE bare `` `:3` `` is two citation groups, and that check only ever
#   looks inside one. Rule 2 now keys its exemption per SPAN rather than per
#   group (sozu-proxy/sozu#1457), which closes the drift half of that identity
#   problem; this is rule 1's half — a duplicate ACROSS two citation groups —
#   and it is still open, here or anywhere. A backtick inside a fenced block is not code-span syntax to
#   CommonMark although it is to this pattern; no such span exists in the
#   guarded surface.
#
# DEAD TEST-NAME CITATIONS
#   The resolver above only sees a citation that carries a path. A second form
#   carries none: prose that names a TEST as its evidence — "`<name>` pinned
#   the defect", "see `<name>` for the exact semantics". When that test is
#   renamed, folded into another, or never lands, the claim survives and points
#   at nothing. No compiler, no test run and no reviewer notices, because the
#   prose and the test live in different pull requests. Three such names, cited
#   in six places, survived every check in this tree until sozu-proxy/sozu#1380.
#
#   The rule: a backticked identifier that LOOKS like a test name, in prose
#   that is talking about tests, must name a `fn` somewhere in the tree.
#
#   Both halves of "looks like a test name" are load-bearing, and both were
#   measured on this tree rather than guessed:
#     * `^[a-z][a-z0-9_]{12,}$` alone yields 358 distinct identifiers over 864
#       sites, nearly all configuration keys (`max_connections_per_ip`), struct
#       fields (`frontend_buffer`) and std methods (`saturating_sub`);
#     * requiring FIVE underscore-separated segments — a sentence, not a noun
#       phrase — cuts that to 21 distinct names, and additionally requiring the
#       enclosing comment block or markdown paragraph to contain the word
#       "test" cuts it to 12.
#   Twelve is small enough to disposition by hand, which is what the two tables
#   below are: NOT_A_TEST for an identifier that is not a test name at all, and
#   RENAMED_TESTS for a test deliberately cited by a name it no longer carries.
#   A RENAMED_TESTS entry must forward to a name that DOES resolve to a `fn`,
#   so the forwarding pointer cannot rot in turn.
#
#   This rule is a floor in the same way the resolver is. A test name of four
#   segments or fewer is not examined; prose that never says "test" is not
#   examined; and a citation that names a `fn` which is not the test the prose
#   means still passes. The alternative is 864 sites of noise, which nobody
#   reads and therefore nobody maintains.
#
# STALE CODE QUOTED IN PROSE
#   Rules 1 to 3 all check a POINTER — a path, a line number, a name. None of
#   them looks at the code a document QUOTES. sozu-proxy/sozu#1424 measured the
#   consequence: `doc/h2_mux_internals.md` carried fenced Rust blocks writing
#   `self.encoder`, `self.decoder`, `self.converter_buf` and `self.lowercase_buf`
#   on `ConnectionH2` months after #1403 moved all four behind
#   `hpack_state.rs`, where they are private. Each would fail to compile, and
#   every check here was green over them — a stale quote beside a RESOLVING
#   citation reads as authoritative, so a reader who copies it assumes the
#   compile error is theirs.
#
#   Two mechanisms were measured on this tree before this rule was written, and
#   both were rejected:
#
#     * COMPARE EVERY ```rust BLOCK AGAINST THE TREE. The guarded surface holds
#       215 fenced blocks, 27 of them Rust. Searching the whole tree for each
#       block's exact line sequence locates 7. The other 20 are not stale —
#       they are abbreviated on purpose (`pub fn readable(&mut self, context,
#       endpoint) -> MuxResult` drops the generics and the `where` clause that
#       would bury the point), composite (one block narrating two distant call
#       sites), or simplified (`pub struct FlowKey { pub src: SocketAddr }`).
#       A rule firing on 20 of 27 blocks is a rule that gets silenced.
#     * RUSTDOC DOCTESTS via `#[doc = include_str!("../../doc/....md")]`. This
#       tree has no `include_str!` of a markdown file and no doctest
#       configuration anywhere, and the reason is structural rather than
#       historical: a doctest is compiled as a SEPARATE crate against the
#       public API. Every snippet at issue sits inside `impl ConnectionH2` and
#       reads a private field of a private type in a private module. No
#       doctest can see any of it. The mechanism cannot reach the exact class
#       that motivated the rule.
#
#   So the rule is OPT-IN, and the annotation is an ordinary citation in the
#   fence's info string:
#
#       ```rust lib/src/protocol/mux/hpack_state.rs:121-131
#
#   The cited lines are read and compared to the block, stripped, line by
#   line. That is zero false positives BY CONSTRUCTION — a block that cannot
#   be quoted verbatim simply carries no annotation — and it gives the author
#   the thing no rule here offered before: a way to say "this is a quote, hold
#   me to it". Because the fence line is part of the document body, rules 1
#   and 2 see the annotation too: rule 1 range-checks a pinned block from the
#   moment it lands, and rule 2 drift-checks it FROM THE NEXT COMMIT ONWARD —
#   not on the commit that introduces it. Rule 2 exempts a citation absent
#   from the base revision of its own document, and on that commit every pin
#   is absent. Measured when the 17 pins in `doc/h2_mux_internals.md` landed:
#   the cited-line total went 219 -> 238 while `compared` stayed at exactly
#   325, so every one of them was exempt that day.
#
#   Be honest about what opt-in costs: a quote nobody pins is a quote nobody
#   checks, and this rule would not have caught #1424's four blocks on its own.
#   What it does is make the repair durable. `doc/h2_mux_internals.md` now pins
#   every block it can, so the next extraction step moves those lines and this
#   rule reports it instead of a reviewer finding it two refactors later.
#
# LINE NUMBERS CITED FROM A RUST COMMENT
#   Rules 1 to 4 read `doc/**` and every `**/LIFECYCLE.md`. A `file.rs:NNN`
#   written in a `//`, `///` or `//!` comment is outside all four, so it was
#   never extracted, never resolved, never drift-compared and never reported as
#   unresolvable either — the same total blindness `BARE CONTINUATIONS`
#   describes, over a much larger surface.
#
#   sozu-proxy/sozu#1473 is the worked case and it is the dangerous shape: two
#   `SAFETY:` comments justifying `from_utf8_unchecked` cited a line that was a
#   `Method` match arm rather than the `hostname_and_port` they named. The
#   claim was TRUE — every byte that function accepts is ASCII — and only the
#   proof path was broken, so a reader auditing the unsafe block followed the
#   pointer, landed somewhere unrelated, and lost confidence the comment was
#   there to give. sozu-proxy/sozu#1466 is the same defect from a third
#   direction: four `h2.rs` citations that were wrong when the drift rule first
#   saw them, and therefore frozen wrong forever, because "unchanged" is
#   exactly the condition for being exempt from rule 2.
#
#   WHY FORBID RATHER THAN RESOLVE. Extending rules 1 and 2 over `**/*.rs` was
#   the obvious alternative and it was measured, not argued. At main `19fd5d8c`
#   the tree carried 90 such citation tokens across 25 files holding 113 line
#   targets, and SIXTY-EIGHT of the 90 had already drifted since the commit
#   that last wrote them; 43 of those 68 broke on the very next commit touching
#   the file they point into, and the median citation survived ONE such commit.
#   Switched on, the resolver would have had to reject all 68, plus 14 naming a
#   basename that is ambiguous here — four files are called `h2.rs`, nineteen
#   `mod.rs` — plus two pointing into the `kawa` dependency. A gate that fails
#   on most of the population it guards gets bypassed rather than satisfied.
#   Worse, it would have widened the DRIFT rule across a codebase where line
#   shifts are constant and legitimate, so an ordinary correct edit would
#   report a failure and the exemption machinery of "A NUMBER REUSED FOR
#   DIFFERENT CODE" would have to carry the whole tree instead of one document
#   set.
#
#   Forbidding costs none of that. The rule asks for no number to be right,
#   only for there to be no number, so it imports not one pre-existing failure
#   and there is nothing for it to renumber. sozu-proxy/sozu#1480 converted the
#   population whole before this rule landed, so it started green — and it
#   stays green by conversion, never by renumbering.
#
#   SCOPE, stated rather than implied, because a rule narrowed until it matches
#   nothing reads exactly like a rule that passes:
#     * EVERY `*.rs` IN THE TREE, not `lib|command|bin|e2e/src` alone. The
#       defect is a property of the form, not of the directory the comment sits
#       in, and a directory list would leave `sim/`, `fuzz/`, `lib/benches`,
#       `lib/examples`, the integration tests and every `build.rs` free to
#       reintroduce it while the rule still read as covering Rust comments.
#     * RESOLVED SIBLING-FIRST, through rule 1's own `resolve_path`, so this
#       rule and the resolver can never disagree about which file a citation
#       names. That is load-bearing and not a detail: `mux/h2.rs` cited its own
#       sibling as a bare `h1.rs:361-368`, and #1473's own finder grep — keyed
#       on a `(lib|command|bin|e2e)/src/` prefix — walked straight past it. A
#       rule built on that same prefix would have shipped green over a live
#       instance of the exact defect it was written for. An AMBIGUOUS basename
#       fails too: it still names lines this tree's own edits move.
#     * A CITATION NAMING NO FILE HERE IS SKIPPED AND COUNTED. `stream.rs`
#       cites `kawa/src/protocol/h1/parser/primitives.rs:194-203` into a
#       dependency pinned at a version; nothing in this repository can move
#       those lines, so this rule is not their owner. `doc/README.md` reached
#       the same verdict on the same two when the convention landed.
#     * `CHANGELOG.md` IS NOT READ, and that is deliberate rather than
#       incidental — it is `*.md`, and the rule reads `*.rs`. It is an
#       append-only record of the tree as it stood at each release, carrying
#       quoted tool transcripts of what a command printed on the day it ran. 27
#       of its citations would fail a resolver (measured on `265d895d`), and a
#       rule that made them fail would be asking for a record to be rewritten.
#     * A TRAILING COMMENT ON A CODE LINE IS NOT READ. See `RUST_COMMENT_LINE`
#       for the measurement: all eight tokens in the tree sat on a
#       comment-leading line, and reading "everything after the first `//`"
#       would have bought zero sites and cost the `//` inside a string literal.
#
#   THE ONE EXEMPTION, and what it is not for. A comment that records a
#   position AT A REVISION THAT NO LONGER EXISTS cannot be converted — the
#   construct it names was deleted, so there is no symbol — and must not be
#   renumbered onto today's tree, because that falsifies a record instead of
#   repairing it. `HISTORICAL_CITATIONS` declares such a site, keyed on its
#   citing file AND its exact citation text, with the reason, exactly as
#   `NOT_A_TEST` does. An entry without a reason is an unreviewed silencing of
#   this rule. A live pointer into a file this repository still edits does not
#   belong there: it has a symbol to name, and naming it is the whole remedy.
#
#   WHAT THIS DOES NOT CATCH. It is not a floor in rules 1 to 4's sense — there
#   is no number left to be subtly wrong — but it says nothing about whether
#   the SYMBOL a comment names is the right one, or still exists. Rule 3 covers
#   that for a cited test name and nothing covers it for an arbitrary symbol; a
#   comment naming a function that was renamed away still passes here. What the
#   rule guarantees is narrower and worth having on its own: no comment in this
#   tree points at a line number, so no comment in this tree can rot by an edit
#   made somewhere above it.
#
# AUDITING A CITATION THAT NEVER CHANGED — `--audit`, AND WHY IT IS NOT A RULE
#   Rules 1 to 4 resolve a citation and rule 5 forbids a form. None of the five
#   can find a citation that was ALREADY WRONG the first time it was seen, and
#   rule 2 cannot by construction: it compares a citation's text between two
#   revisions, so "unchanged" is exactly the condition for being exempt. A
#   change detector cannot find a defect that predates its first observation —
#   sozu-proxy/sozu#1466, which measured four citations in
#   `lib/src/protocol/mux/LIFECYCLE.md` at `5d5191e8` that were byte-identical
#   across the whole series and wrong in every one of them. Three landed on
#   comment lines under prose naming an insert or a push; the fourth,
#   `h2.rs:710`, cited `self.stream_table.rst_sent_contains(sid)` and resolved
#   to `let total_before = *total;`.
#
#   sozu-proxy/sozu#1493 closed that hole for RUST COMMENTS by forbidding the
#   form there outright, which is a complete remedy and needs no heuristic.
#   `doc/**` and `**/LIFECYCLE.md` deliberately keep line numbers, because
#   there a number is sometimes the only way to name a span with no symbol, so
#   that surface needs an audit instead of a prohibition.
#
#   `--audit` is that audit. It reads every citation's PROSE against its cited
#   LINE and reports what does not plainly match, on TWO heuristics:
#
#     * THE TARGET IS A COMMENT while the citing prose names a statement, a
#       call, an insert or a push. Single-line citations only — a RANGE that
#       starts on a comment is this tree's normal way of covering a branch
#       together with the sentence introducing it, and reading ranges too took
#       the mode from 13 findings to 26 at `6172929e`, all 13 additions
#       hand-audited and all 13 correct.
#     * THE PROSE NAMES A SYMBOL THAT IS NOT THERE — not within
#       AUDIT_WINDOW lines of the cited span, and not as an item enclosing it.
#       That is the one that reaches #1466's fourth citation, which lands on
#       ordinary code and looks healthy to everything else here.
#
#   IT IS ADVISORY, DELIBERATELY, AND IT IS NOT IN `ci.yml`. It always exits 0
#   and it never edits a citation. A heuristic that fails a build is a
#   heuristic people learn to silence, and the silencing outlives the reason;
#   every rule above earns its exit code by resolving something, and none of
#   these two resolves anything. What this produces is a LIST A HUMAN
#   DISPOSITIONS — `wrong`, `correct`, or `false positive` with the reason —
#   and the disposition is the deliverable, not the list.
#
#   WHAT ITS FIRST RUN COST AND BOUGHT, so nobody has to guess at the rate.
#   At main `6172929e` it examined 108 of the surface's 221 cited spans and
#   reported 13. Hand-audited, SIX were wrong citations and SEVEN were false
#   positives — a little under half. That ratio is the honest price of the
#   mode and it is printed in its own output. The six:
#     * `doc/testing.md`'s `redirect_rewrite_auth_tests.rs:264`, the middle
#       line of a three-line comment whose check is two lines below it;
#     * `doc/testing.md`'s `h2_security_tests.rs:2440`, a comment four lines
#       above the `0x20` byte the prose says is sent;
#     * `kawa_h1/LIFECYCLE.md`'s `lib/src/tcp.rs:2426`, a bare `}`, where
#       `fn handle_connection_result` is at 2479 — repaired by dropping the
#       number, since the prose already names the symbol;
#     * three in `mux/LIFECYCLE.md` naming `Mux::shutting_down` while pointing
#       into `shutting_down_inner`, the body that the four-line `SessionState`
#       wrapper drives.
#
#   THE FALSE POSITIVES ARE DOCUMENTED, NOT TUNED AWAY. Two classes were
#   tightened because the heuristic was simply wrong — a range that starts on a
#   comment, and a symbol read across a bare-path citation belonging to another
#   clause. The rest are left reporting, with the reason, because narrowing
#   further would start costing real findings:
#     * A SYMBOL THE PROSE NAMES AS A QUALIFIER, not as the cited construct —
#       "Index of 232 with the parser still `Incomplete` (`expect.rs:249-259`)".
#       The citation is right and the name belongs to the sentence, not the
#       line.
#     * A NAME THAT IS ONLY A COMPONENT of the identifier carrying it:
#       `ClusterConfiguration` is present at `request_builder.rs:391-397` only
#       inside `find_cluster_configuration`, which a whole-word search cannot
#       see.
#     * A NAME THE CITED LINE IS ABOUT BY ABSENCE: "remove `READABLE` interest
#       (`h2.rs:4561`)" points at the mask that leaves `READABLE` out.
#     * A DECLARATION FURTHER THAN AUDIT_WINDOW inside the construct the prose
#       names — `h2.rs:2901`'s loop header is 16 lines above it.
#
#   AND WHAT IT CANNOT SEE AT ALL, counted in its own output beside what it
#   checked, because silence about skipped work is the defect behind
#   sozu-proxy/sozu#1457 and sozu-proxy/sozu#1447. The largest by far is PROSE
#   THAT NAMES NOTHING TESTABLE — 111 citations at `6172929e`, more than half
#   the surface — a sentence with no backticked symbol and none of the
#   statement nouns, inside which a wrong citation is invisible here. Two more
#   it declines by design: a prose-to-prose citation, which has no code shape
#   to read, and any span rule 1 already rejects. And one it MISSED while
#   looking straight at it: `mux/LIFECYCLE.md`'s `Mux::timeout` citation carried
#   the same defect as the three `shutting_down` ones above — its number pointed
#   into `timeout_inner` — and the audit did not report it, because the word
#   `timeout` appears in a `trace!` and a comment within the window. It was
#   found by hand while dispositioning the finding on the bullet directly below
#   it, and repaired in the same changeset. Proximity cannot tell "the name is
#   nearby" from "the item is here".
#
#   WIDENING THE PROSE WINDOW WAS MEASURED AND REJECTED — sozu-proxy/sozu#1531.
#   That issue filed a citation in `doc/lifetime_of_a_session.md` that named
#   `accept_queue.saturated_seconds` while pointing at a buffer-pool gauge
#   block fifteen lines above the ticker, and asked whether this mode could be
#   made to catch it. It cannot usefully, and the reason is worth keeping.
#
#   The signal it needs already exists — "the prose names a symbol that is not
#   there". What silenced it is `audit_fragment`: the citation sits mid-line
#   under the sentence naming the metric, so the fragment is its own line
#   alone, which carries no symbol, and BOTH of that document's spans were
#   DECLINED as "prose naming nothing this heuristic can test" rather than
#   examined. So the candidate is not a new rule, it is a wider prose window.
#   Measured at `00118f77`, against this mode's own 7 findings / 81 examined:
#
#     * ALWAYS READ ONE LINE ABOVE (keeping the nearest-symbol picker): 13
#       findings, +6 and -0. ONE of the six is a real defect. It also reaches
#       only ONE of the issue's two citations: at the metric-inventory one,
#       `audit_symbol` walks back onto `SessionManager::decr`, whose last
#       segment is four characters, and RETURNS None instead of continuing to
#       `check_limits` — a second silencer, unrelated to the window.
#     * THE ISSUE'S LITERAL RULE — any identifier the surrounding prose names,
#       flagging only when NONE is present: 17 findings, +13 and -3. TWO of the
#       thirteen are real, both of them the filed defect. The three it loses
#       are correct citations this mode reports today.
#     * BOTH, over a whole blank-line-delimited block: 26 findings, +23 and -4,
#       with no further real defect than those two.
#
#   So the honest rate on the additions is 1 in 6, or 2 in 13, against the 6 in
#   13 this mode measured for itself and PRINTS in its own banner. It would
#   also re-import false-positive classes already measured out above: reading
#   every symbol in the fragment reinstates `expect.rs:219-236` / `ProxyAddr`,
#   quoted verbatim in `audit_symbol` as the reason the nearest one is read,
#   and a block-wide window merges adjacent markdown table rows and reads a
#   fenced block's `rust` info string as a prose symbol. A mode whose banner
#   has to promise one finding in six is a mode reviewers stop reading, so the
#   item was closed rather than shipped.
#
#   AND THE VARIANTS THAT DO FIRE ON IT FIRE BY A ONE-LINE MARGIN, which is
#   the real reason to stop. The wrong span ends seven lines above the ticker's
#   own comment, and that comment names the metric; AUDIT_WINDOW is 8, so the
#   name falls outside by exactly one line. At 9 it is inside and every variant
#   above goes quiet while the citation stays just as wrong. The correct
#   companion span in the same sentence is silent for the very same reason in
#   reverse — its first line is the constant's doc comment, which names the
#   metric. Same mechanism, opposite verdicts, one line apart: this is
#   `Mux::timeout` again, and proximity still cannot tell "the name is nearby"
#   from "the item is here".
#
#   WHAT DID FIX IT is the convention this file already states: the five spans
#   that issue's changeset repaired were replaced by symbols, and the declined
#   count fell from 95 to 90 — every one of them had been in the bucket this
#   mode cannot read. A symbol needs no window.
#
# Usage:
#   python3 .github/scripts/check_doc_citations.py            # check the tree
#   python3 .github/scripts/check_doc_citations.py --show     # + print every
#                                                             #   resolved citation
#   python3 .github/scripts/check_doc_citations.py --self-test # prove it fails
#   python3 .github/scripts/check_doc_citations.py --audit     # advisory: read
#                                                              #   every citation
#                                                              #   against its prose
#
# Standard library only, on purpose: this is tooling, and sozu's production
# dependency set does not grow for tooling.

import argparse
import os
import re
import shutil
import subprocess
import sys
import tempfile

# One citation "group" is a path plus its first line reference, optionally
# followed by more line references introduced by `,` or `/`:
#   lib/src/tcp.rs:1563-1567, 1830-1835
#   lib/src/protocol/mux/answers.rs:209/224
#   doc/configure.md:1133, 1234
# A continuation may wrap a line in the prose, so the separator swallows a
# newline plus that line's leading whitespace. The extension alternation is
# non-capturing on purpose: `match.group("path")` is read by name, but a
# numbered group here would still renumber anything added after it.
# A continuation written as a separate code span carries no path at all
# (`` `lib/src/tcp.rs:183` and `:305` ``); that form is `BARE_CITATION` below
# and it is bound to a path by `citations()`, never by this pattern.
PATH = r"[A-Za-z0-9_][A-Za-z0-9_./-]*\.(?:rs|md)"
SPAN = r"[0-9]+(?:-[0-9]+)?"
CITATION = re.compile(
    rf"(?P<path>{PATH}):(?P<spans>{SPAN}(?:[,/][ \t]*(?:\n[ \t]*)?{SPAN})*)"
)
SPAN_SEP = re.compile(r"[,/][ \t]*(?:\n[ \t]*)?")
# A CONTINUATION THAT NAMES NO PATH: `` `:305` ``, written after a full citation
# to name a second site in the same file without repeating its path. `CITATION`
# requires a path, so until sozu-proxy/sozu#1459 this form was extracted by
# nothing and therefore checked by nothing. See "BARE CONTINUATIONS" in the
# header for the two discriminators below and the measurements behind them: the
# whole code span must be a colon and one span GROUP, which is what separates
# the three real ones from 154 ports, IPv6 addresses, log timestamps and statsd
# lines in the same documents.
#
# The span group mirrors `CITATION`'s own, so `` `:3/5` `` and `` `:3, 5` `` are
# extracted exactly as `` `:3` `` and `` `:3-5` `` are. Writing `{SPAN}` alone
# here reads as the narrower, safer choice and is the opposite: this tree uses
# `/` groups heavily (`answers.rs:209/224`, `mod.rs:1218/1233/1311/1321`), so a
# bare one is a form an author will reach for, and a pattern that does not match
# it puts that citation back where #1459 found it — extracted by nothing,
# checked by nothing, and not reported as unresolvable either. There is no
# `\n` in it, unlike `CITATION`'s: a continuation binds on its own line, so a
# group that wrapped could not bind anyway.
BARE_CITATION = re.compile(rf"`:(?P<spans>{SPAN}(?:[,/][ \t]*{SPAN})*)`")
# `testdata` holds this script's own fixtures, including a deliberately broken
# document and a `sample.rs` that must never answer a real citation. Skipping
# the directory by name keeps it out of the repository walk while leaving the
# self-test — which roots the same walk *inside* it — unaffected.
SKIP_DIRS = {".git", "target", "node_modules", "testdata"}


# Every extension a citation may TARGET. `.md` is in it because prose cites
# prose; see "MARKDOWN TARGETS" in the header for the six wrong ones that
# measured the gap.
#
# This tuple feeds ONLY the tree-wide suffix index, which is `resolve_path`'s
# third and last branch. A citation that names a sibling of its own document,
# or names a path from the repository root, is answered by the two `isfile`
# branches ahead of it and never consults this index at all — so most markdown
# citations resolve identically whatever is in here, and an entry removed from
# it shrinks the guarded set in silence.
#
# `doc/good.md`'s bare `LIFECYCLE.md:8` is the fixture that makes that audible:
# it is the one citation in the tree that reaches the third branch, so removing
# `.md` here turns it into `no such file in the tree` and fails the self-test
# four ways. Before it existed this tuple was asserted by nothing — reverting
# it alone left the self-test green at exit 0 — while the three assertions that
# looked like its guard all belonged to PATH. Keep a citation that resolves
# only through the index, or this constant is decoration again.
TARGET_SUFFIXES = (".rs", ".md")


def target_files(root):
    """Every citable path in the tree, repo-root-relative, with a suffix index.

    Citable is `*.rs` and `*.md`, matching `PATH`. Both have to be in the index
    `resolve_path` falls back on, or a markdown citation resolves only in the
    two cases that never reach the index — a target sitting beside the citing
    document, or one named repo-root-relative — and a correct citation to a
    document elsewhere in the tree is reported as missing.
    """
    by_suffix = {}
    for dirpath, dirnames, filenames in os.walk(root):
        dirnames[:] = [d for d in dirnames if d not in SKIP_DIRS]
        for name in filenames:
            if not name.endswith(TARGET_SUFFIXES):
                continue
            rel = os.path.relpath(os.path.join(dirpath, name), root).replace(os.sep, "/")
            parts = rel.split("/")
            for i in range(len(parts)):
                by_suffix.setdefault("/".join(parts[i:]), []).append(rel)
    return by_suffix


def doc_files(root):
    """`doc/**` markdown plus every `**/LIFECYCLE.md` — the guarded surface.

    CHANGELOG.md is NOT in this surface, on purpose per rule 1/2's own scope,
    but that means its `file.rs:NNN` citations get no rule-1 blank check and
    no rule-2 drift check either — only rule 3's narrower test-name sweep
    reaches it. Measured on this tree with this module's own `CITATION`
    regex: 108 matches in CHANGELOG.md (105 unique `(path, spans)` pairs),
    none of them checked by anything. A green run here proves nothing about
    that file's line-number citations.
    """
    found = []
    for dirpath, dirnames, filenames in os.walk(root):
        dirnames[:] = [d for d in dirnames if d not in SKIP_DIRS]
        for name in filenames:
            rel = os.path.relpath(os.path.join(dirpath, name), root).replace(os.sep, "/")
            if name == "LIFECYCLE.md" or (rel.startswith("doc/") and name.endswith(".md")):
                found.append(rel)
    return sorted(found)


def resolve_path(cited, by_suffix, root, doc_dir):
    """Map a cited path to exactly one file, or explain why it cannot.

    Order matters. A module `LIFECYCLE.md` cites its own siblings by bare name
    — `h2.rs:NNN` inside `lib/src/protocol/mux/` means that directory's
    `h2.rs`, not `bin/src/ctl/top/panes/h2.rs` — so the document's own
    directory is tried first. Repo-root-relative is next, and a unique
    tree-wide suffix match last; anything still matching several files is
    reported as ambiguous rather than guessed at.
    """
    if doc_dir:
        beside = "%s/%s" % (doc_dir, cited)
        if os.path.isfile(os.path.join(root, beside)):
            return beside, None
    if os.path.isfile(os.path.join(root, cited)):
        return cited.replace(os.sep, "/"), None
    matches = sorted(set(by_suffix.get(cited, [])))
    if len(matches) == 1:
        return matches[0], None
    if not matches:
        return None, "no such file in the tree"
    return None, "ambiguous basename, resolves to %d files (%s) — make it repo-root-relative" % (
        len(matches),
        ", ".join(matches[:4]) + (", …" if len(matches) > 4 else ""),
    )


def doc_line_finder(body):
    """Map a citation's match offset in `body` to the 1-based line it sits on.

    Shared by the resolver and the drift rule so the two never disagree about
    where in a document they are reporting from.
    """
    starts = [0]
    for line in body.splitlines(keepends=True):
        starts.append(starts[-1] + len(line))

    def doc_line(offset):
        lo, hi = 0, len(starts) - 1
        while lo < hi:
            mid = (lo + hi + 1) // 2
            if starts[mid] <= offset:
                lo = mid
            else:
                hi = mid - 1
        return lo + 1

    return doc_line


def parse_spans(text):
    """`1563-1567, 1830-1835` -> [(1563, 1567), (1830, 1835)]."""
    out = []
    for piece in SPAN_SEP.split(text):
        piece = piece.strip()
        if not piece:
            continue
        if "-" in piece:
            a, b = piece.split("-", 1)
            out.append((int(a), int(b)))
        else:
            out.append((int(piece), int(piece)))
    return out


def citations(body):
    """Every citation in `body`, in document order, with bare ones bound.

    Yields `(path, spans_text, spans, offset)`: the cited path, the normalized
    span text a report quotes, the parsed spans, and the match offset the
    caller turns into a document line with `doc_line_finder`.

    `path` is `None` for a BARE continuation that found nothing to inherit
    from, which is a reported failure rather than a skip — a silent one is the
    exact defect this form carried until sozu-proxy/sozu#1459.

    Rule 1 and rule 2 both read documents through this function, base revision
    included, so the two can never disagree about what a document cites and a
    bare continuation's drift identity is its inherited `(path, spans)` exactly
    as a written-out one's is.
    """
    found = [
        (m.start(), m.end(), m.group("path"), m.group("spans"))
        for m in CITATION.finditer(body)
    ]
    # `PATH` admits no backtick, so a bare span can never sit inside a full
    # citation's match today. The guard is here so that widening `PATH` later
    # cannot silently count one span twice.
    written = [(start, end) for start, end, _, _ in found]
    for match in BARE_CITATION.finditer(body):
        if any(start < match.end() and match.start() < end for start, end in written):
            continue
        found.append((match.start(), match.end(), None, match.group("spans")))
    found.sort()

    binder = None  # (end offset, path) of the last full citation seen
    for start, end, path, spans_text in found:
        if path is not None:
            binder = (end, path)
        elif binder is not None and "\n" not in body[binder[0]:start]:
            # SAME LINE and no wider: see "BARE CONTINUATIONS" for why the
            # blank-line paragraph those three sites sit in is 15, 16 and 101
            # lines long, and what a carry that wide would bind.
            path = binder[1]
        yield path, " ".join(spans_text.split()), parse_spans(spans_text), start


def check(root, show=False, out=sys.stdout):
    by_suffix = target_files(root)
    cache = {}
    failures = []
    total = 0

    for doc in doc_files(root):
        doc_dir = os.path.dirname(doc)
        with open(os.path.join(root, doc), encoding="utf-8") as handle:
            body = handle.read()
        # Line number of the citation itself, for the error message.
        doc_line = doc_line_finder(body)

        for cited, spans_text, spans, offset in citations(body):
            total += 1
            where = "%s:%d" % (doc, doc_line(offset))
            if cited is None:
                # A bare continuation with nothing on its line to inherit from.
                # Reported rather than skipped: the author always has the path,
                # and a skip here is the hole #1459 measured.
                failures.append(
                    "%s: `:%s` — a bare continuation with no citation earlier on "
                    "its own line to inherit a path from; write the path out"
                    % (where, spans_text)
                )
                continue

            target, why = resolve_path(cited, by_suffix, root, doc_dir)
            if target is None:
                failures.append("%s: `%s:%s` — %s" % (where, cited, spans_text, why))
                continue

            if target not in cache:
                with open(os.path.join(root, target), encoding="utf-8") as handle:
                    cache[target] = handle.read().splitlines()
            lines = cache[target]

            # A repeated line inside one group is a half-applied renumbering:
            # `answers.rs:209/224` rewritten on its first half alone yields
            # `224/224`, which resolves and would otherwise pass unseen.
            seen = set()
            for start, end in spans:
                if (start, end) in seen:
                    failures.append(
                        "%s: `%s:%s` — line %s repeated inside one citation; a "
                        "`/` or `,` continuation was renumbered on one half only"
                        % (where, cited, spans_text, start if start == end else "%d-%d" % (start, end))
                    )
                    break
                seen.add((start, end))

            for start, end in spans:
                span = str(start) if start == end else "%d-%d" % (start, end)
                if start > end:
                    failures.append(
                        "%s: `%s:%s` — inverted range" % (where, cited, span)
                    )
                    continue
                if start < 1:
                    failures.append(
                        "%s: `%s:%s` — line numbers start at 1" % (where, cited, span)
                    )
                    continue
                if end > len(lines):
                    failures.append(
                        "%s: `%s:%s` — past end of %s (%d lines)"
                        % (where, cited, span, target, len(lines))
                    )
                    continue
                if not lines[start - 1].strip():
                    failures.append(
                        "%s: `%s:%s` — %s:%d is blank"
                        % (where, cited, span, target, start)
                    )
                    continue
                # Both ends of a range must be real lines. Interior lines are
                # deliberately NOT checked: a span that deliberately covers a
                # whole branch routinely contains blank lines — 26 of them
                # across the guarded surface — so requiring every line to be
                # non-blank would reject correct citations.
                if end != start and not lines[end - 1].strip():
                    failures.append(
                        "%s: `%s:%s` — %s:%d is blank (end of range)"
                        % (where, cited, span, target, end)
                    )
                    continue
                if show:
                    out.write("%s  %s:%s  |%s\n" % (where, target, span, lines[start - 1]))

    return total, failures


# ── Rule 2: drifted citations ─────────────────────────────────────────────
#
# See "DRIFTED CITATIONS" in the header for the measurements this rule closes.

# A quoted line is clipped to this many characters, its ellipses included: a
# quoted line of Rust, or a markdown table row, can be very long, and this is
# enough to recognise the construct without wrapping the log. It bounds the
# WIDTH of the window and says nothing about where that window sits — which is
# `quote_pair`'s business, and the reason this constant never had to be raised.
QUOTE_WIDTH = 72


def git(root, *args):
    """Run one git command in `root`; return `(returncode, stdout)`.

    stderr is deliberately dropped: every caller here treats a non-zero exit as
    the answer ("that object is not in this checkout"), and git's own wording
    for a missing object would only obscure the report this script writes.

    The encoding is named rather than inherited, exactly as every `open()` here
    names it: a document under `doc/` is full of em dashes, and decoding git's
    output through the ambient locale would make this rule's verdict depend on
    the runner's `LANG`.
    """
    proc = subprocess.run(
        ["git", "-C", root] + list(args),
        capture_output=True, text=True, encoding="utf-8",
    )
    return proc.returncode, proc.stdout


def shallow_note(root):
    """The clause that names a shallow checkout as the cause, when it is one."""
    code, out = git(root, "rev-parse", "--is-shallow-repository")
    if code == 0 and out.strip() == "true":
        return (
            "; this checkout is SHALLOW, so the base revision is simply not in it — "
            "fetch it (actions/checkout `fetch-depth: 0`)"
        )
    return ""


def resolve_base(root, base):
    """Turn `--base REV` into the commit the drift rule reads, or explain.

    Returns `(sha, None)` or `(None, reason)`. The caller must treat a reason
    as a FAILURE, never as "nothing to compare": the default `actions/checkout`
    is shallow and leaves the base out of the object store entirely, so a guard
    that skipped on an unreachable base would report a clean run over an empty
    comparison — which is the shape of defect this whole file exists to close.
    """
    code, out = git(root, "rev-parse", "--verify", "--quiet", "%s^{commit}" % base)
    if code != 0:
        return None, "`%s` names no commit in this checkout%s" % (base, shallow_note(root))
    resolved = out.strip()
    code, out = git(root, "merge-base", "HEAD", resolved)
    if code != 0:
        return None, "HEAD and `%s` have no common ancestor here%s" % (
            base,
            shallow_note(root),
        )
    return out.strip(), None


def blob(root, rev, path, cache):
    """`path` as it stood at `rev`, or None when that tree did not carry it."""
    key = (rev, path)
    if key not in cache:
        code, text = git(root, "show", "%s:%s" % (rev, path))
        cache[key] = text if code == 0 else None
    return cache[key]


def _window_start(was, now):
    """The offset BOTH quotes are clipped from, chosen so the difference shows.

    Column 0 is right whenever the two lines diverge inside the first window:
    a leading ellipsis would cost a character and buy nothing. Past that, a
    left-anchored window renders the two revisions to the SAME string, and the
    report fires correctly while naming nothing that moved. A markdown table
    row is the canonical case — its meaning sits in the last column and its
    first 72 characters are a padded name and a default (sozu-proxy/sozu#1448)
    — but nothing here knows that: the window follows the difference, so a
    Rust line whose only edit is far to the right is served the same way and
    no rule needs per-file-type knowledge to get a readable report.

    Half a window of the common prefix is kept ahead of the difference, so the
    reader still sees what the changed text is part of.
    """
    shared = 0
    limit = min(len(was), len(now))
    while shared < limit and was[shared] == now[shared]:
        shared += 1
    # A clipped left-anchored quote shows offsets 0 to QUOTE_WIDTH - 2; the
    # last character is its ellipsis. A difference inside that range is already
    # visible, and so is the case where one line is a prefix of the other.
    if shared < QUOTE_WIDTH - 1:
        return 0
    return shared - QUOTE_WIDTH // 2


def _clip(line, start):
    """`line` from `start`, never wider than QUOTE_WIDTH, ellipses included."""
    head = "…" if start > 0 else ""
    span = QUOTE_WIDTH - len(head)
    if start + span < len(line):
        return head + line[start : start + span - 1] + "…"
    return head + line[start:]


def quote_pair(was, now):
    """One BEFORE/AFTER pair, both clipped around their first difference.

    Every quote this script prints is half of such a pair — rule 2's drifted
    line and rule 4's stale block line, neither of which is ever quoted alone —
    so the window is chosen from the two together rather than from either one.
    Clipping each of them from column 0 prints two identical strings whenever
    they diverge past the clip, which is a report a reviewer learns nothing
    from (sozu-proxy/sozu#1448).

    Both are stripped, matching what the two rules compare, and both are
    clipped from the SAME offset so they can be read against each other.
    Neither result ever exceeds QUOTE_WIDTH: an unbounded quote in a CI log is
    a different failure, and moving the window is exactly what makes widening
    it unnecessary. Both callers have already established that the two lines
    differ; equal lines would simply be clipped around their common end.
    """
    was, now = was.strip(), now.strip()
    if len(was) <= QUOTE_WIDTH and len(now) <= QUOTE_WIDTH:
        return was, now
    start = _window_start(was, now)
    return _clip(was, start), _clip(now, start)


def span_ends(start, end):
    """The line numbers a span is CHECKED at: one, or the two ends of a range.

    Rule 1 requires both ends of a range to be non-blank and leaves the
    interior alone, and rule 2 compares exactly the same two lines so it stays
    a strict superset of it. Every place here that walks a span walks this.
    """
    return (start,) if start == end else (start, end)


def reanchored_claims(root, base, base_body, head_body, by_suffix, doc_dir, blobs, head):
    """Every cited line TEXT this changeset carried from an old number onto a new one.

    This is rule 2's identity for a citation, and it is the answer to
    sozu-proxy/sozu#1447: a span NUMBER is not stable under the edits the rule
    exists to police, so keying the exemption on the number alone reports a
    CORRECT citation as drift whenever the changeset renumbers one onto a line
    the base revision had spent on something else. The number is not the
    citation. The line it names is.

    Returns `{target: {text: number}}` — for each cited file, every line text
    whose claim demonstrably MOVED, mapped to the new number that now carries
    it. `check_drift` reads it in exactly one place: a span whose text changed
    is declined instead of reported when the text it held at the BASE is in
    this map, because the base document's claim on that text is alive at a
    number this changeset introduced, and the span under test is therefore a
    different citation that merely inherited the number.

    NOTHING HERE GUESSES, and the two conditions are what keep it that way:

      * a RECEIVER is a head-cited end whose number this document did not cite
        at the base revision at all. A number the base already carried explains
        nothing — it is as likely to be a second stale citation as a new one,
        which is the two-adjacent-citations case where a file shrinks by a line
        and each citation lands on its neighbour's old text.
      * the receivers for a text must be AT LEAST AS MANY as the base
        citations that named it. A document that named one line twice and
        re-anchored one of the two has left the other behind, and that is
        precisely the half-applied renumbering of sozu-proxy/sozu#1457, which
        this rule must keep reporting. `doc/drift.md` is that fixture: its base
        revision cites `drift.rs:12` twice, this changeset re-anchors one of
        them to `:16` and leaves the other, so one claim is unaccounted for and
        the stale span is compared as before.

    Both are counted, not inferred, and a match is an exact text equality
    between two revisions of the same file — never a similarity.
    """
    base_ends = {}
    claims = {}
    for cited, _text, spans, _offset in citations(base_body):
        if cited is None:
            continue  # rule 1 reports a continuation that binds to nothing
        target, _ = resolve_path(cited, by_suffix, root, doc_dir)
        if target is None:
            continue  # rule 1 reports an unresolvable path
        base_lines = blob(root, base, target, blobs)
        if base_lines is None:
            continue
        base_lines = base_lines.splitlines()
        for start, end in spans:
            for number in span_ends(start, end):
                base_ends.setdefault(target, set()).add(number)
                if 1 <= number <= len(base_lines):
                    text = base_lines[number - 1].strip()
                    claims.setdefault(target, {})
                    claims[target][text] = claims[target].get(text, 0) + 1

    receivers = {}
    for cited, _text, spans, _offset in citations(head_body):
        if cited is None:
            continue
        target, _ = resolve_path(cited, by_suffix, root, doc_dir)
        if target is None or target not in claims:
            continue  # nothing at the base named a line in this file
        if target not in head:
            with open(os.path.join(root, target), encoding="utf-8") as handle:
                head[target] = handle.read().splitlines()
        head_lines = head[target]
        for start, end in spans:
            for number in span_ends(start, end):
                if number in base_ends.get(target, ()):
                    continue  # not a number this changeset introduced
                if not 1 <= number <= len(head_lines):
                    continue  # rule 1 owns out-of-range at HEAD
                text = head_lines[number - 1].strip()
                receivers.setdefault(target, {}).setdefault(text, []).append(number)

    moved = {}
    for target, texts in receivers.items():
        for text, numbers in texts.items():
            claimed = claims[target].get(text, 0)
            if claimed and len(numbers) >= claimed:
                # The lowest, so a document that re-anchored one text onto
                # several numbers names the same one on every run.
                moved.setdefault(target, {})[text] = min(numbers)
    return moved


def check_drift(root, base, show=False, out=sys.stdout):
    """A citation this changeset left alone must still name the same line TEXT.

    Returns `(compared, exempt, reused, failures)`: how many cited line ENDS
    were resolvable at both revisions and therefore actually compared, how many
    were resolvable and skipped because this changeset re-anchored their SPAN,
    the ones it declined because this changeset re-anchored the CLAIM off their
    number and spent the number on different code, and the drifts among the
    compared ones. The three partition every cited end that resolved and was in
    range at HEAD, which is what makes the second and third readable:
    "38 compared, 12 exempt" says there is something to disposition where
    "38 compared" reads as complete coverage.

    `reused` is a list of report lines rather than a bare count because each
    one is PRINTED, with the text that was matched and the number the claim
    moved to. It is the one class here decided by comparing TEXT across
    revisions rather than by a number's presence, so a reviewer has to be able
    to read the evidence and disagree with it; a count alone would be a
    heuristic with no audit trail.

    Resolution is rule 1's own `resolve_path` on the HEAD tree, so a bare
    `mod.rs` in `mux/LIFECYCLE.md` binds to that directory's sibling exactly as
    it does above; the resolved path is then read at `base` and at HEAD and the
    two lines compared. Comparison is on the STRIPPED line, so a re-indent is
    not drift. Both ENDS of a range are compared, and interior lines are not,
    matching the blank-line rule so this stays a strict superset of it.

    Four things are deliberately not compared, each because rule 1 already
    owns it or because there is nothing to compare against: a SPAN absent from
    the base revision of its own document (the author re-anchored that span), a
    span whose NUMBER the base revision spent on a claim this changeset carried
    somewhere else (the author reused the number), a document or a cited file
    that the base tree did not carry (both are new here), and a line number out
    of range at either revision. The first two are counted and the second is
    printed line by line; the other two have their own owners.

    The exemption is keyed per SPAN, so a group whose first number moved still
    has its siblings compared (sozu-proxy/sozu#1457), and it is keyed on the
    cited TEXT wherever a number alone answers wrong: see "A NUMBER REUSED FOR
    DIFFERENT CODE".
    """
    by_suffix = target_files(root)
    blobs = {}
    head = {}
    failures = []
    reused = []
    compared = 0
    exempt = 0

    for doc in doc_files(root):
        base_body = blob(root, base, doc, blobs)
        if base_body is None:
            continue  # the document itself is new in this changeset
        # Identity is the citation, not where it sits: a paragraph that moved
        # still carries the same claim, so moving it does not excuse a stale
        # number. Keying on the PARSED spans rather than their text also makes
        # `3/6` and `3, 6` the same citation, which they are.
        #
        # The key is ONE SPAN, never the whole group. Keying the group meant
        # that correcting a single number in `h2.rs:5676/5860/5885/5903`
        # changed the tuple, so every sibling was classified "re-anchored" and
        # none was compared however stale it was. That is sozu-proxy/sozu#1457,
        # and it is the exact half-applied renumbering this file exists to
        # catch: the more careful the partial fix looked, the more numbers it
        # hid. Per span, the author who fixes one number buys no exemption for
        # the rest, and `3/6` is still the same citation as `3, 6`.
        untouched = {
            (path, span)
            for path, _text, spans, _offset in citations(base_body)
            if path is not None
            for span in spans
        }

        doc_dir = os.path.dirname(doc)
        with open(os.path.join(root, doc), encoding="utf-8") as handle:
            body = handle.read()
        doc_line = doc_line_finder(body)

        # The other half of the identity, and the half a NUMBER cannot carry:
        # every cited line text whose claim this changeset moved onto a number
        # it introduced. A span whose text changed is DECLINED rather than
        # reported when its base text is in here, because the claim the base
        # document attached to that number is alive somewhere else and the span
        # under test is a different citation that inherited the number
        # (sozu-proxy/sozu#1447).
        moved_claims = reanchored_claims(
            root, base, base_body, body, by_suffix, doc_dir, blobs, head
        )

        for cited, _spans_text, spans, offset in citations(body):
            if cited is None:
                continue  # rule 1 reports a continuation that binds to nothing

            target, _ = resolve_path(cited, by_suffix, root, doc_dir)
            if target is None:
                continue  # rule 1 reports an unresolvable path
            base_lines = blob(root, base, target, blobs)
            if base_lines is None:
                continue  # the cited file is new in this changeset
            base_lines = base_lines.splitlines()
            if target not in head:
                with open(os.path.join(root, target), encoding="utf-8") as handle:
                    head[target] = handle.read().splitlines()
            head_lines = head[target]

            where = "%s:%d" % (doc, doc_line(offset))
            for start, end in spans:
                span = str(start) if start == end else "%d-%d" % (start, end)
                edges = [(start, "")] if start == end else [(start, ""), (end, " (end of range)")]
                # Per span, so a sibling this changeset moved does not cover
                # for one it left behind.
                anchored = (cited, (start, end)) in untouched
                for number, edge in edges:
                    if not 1 <= number <= len(head_lines):
                        continue  # rule 1 owns out-of-range at HEAD
                    if not anchored:
                        # Re-anchored by this changeset, which is not drift —
                        # but COUNTED. A compared total reports what the rule
                        # looked at and never what it declined to look at, so
                        # this rule's coverage could fall to zero for a whole
                        # changeset with every counter it emits still healthy
                        # (sozu-proxy/sozu#1447).
                        exempt += 1
                        if show:
                            out.write("%s  %s:%d  |re-anchored\n" % (where, target, number))
                        continue
                    if number > len(base_lines):
                        continue  # no line at the base revision to compare against
                    was = base_lines[number - 1].strip()
                    now = head_lines[number - 1].strip()
                    moved_to = moved_claims.get(target, {}).get(was)
                    if was != now and moved_to is not None:
                        # The number was REUSED. What the base document claimed
                        # at this number is cited again at a number this
                        # changeset introduced, so the span under test is not
                        # the base citation left behind — it is a different one
                        # that inherited the number, and comparing it reports a
                        # citation no edit of the document can fix. Declined,
                        # never silent: this is the one class here decided on
                        # text rather than on a number's presence, so it is
                        # printed with both.
                        reused.append(
                            "%s: `%s:%s` — %s:%d not compared: the base named `%s` there, "
                            "and this changeset re-anchored that line to %s:%d"
                            % (where, cited, span, target, number,
                               _clip(was, 0), target, moved_to)
                        )
                        if show:
                            out.write("%s  %s:%d  |reused-number\n" % (where, target, number))
                        continue
                    compared += 1
                    if was == now:
                        if show:
                            out.write("%s  %s:%d  |unmoved\n" % (where, target, number))
                        continue
                    was_quoted, now_quoted = quote_pair(was, now)
                    failures.append(
                        "%s: `%s:%s` — %s:%d moved%s: was `%s`, now `%s`"
                        % (where, cited, span, target, number, edge, was_quoted, now_quoted)
                    )

    return compared, exempt, reused, failures


# ── Rule 3: dead test-name citations ──────────────────────────────────────
#
# See "DEAD TEST-NAME CITATIONS" in the header for why this exists and how the
# two filters below were calibrated.

# A backticked identifier that could be a Rust item name. The `{12,}` floor is
# sozu-proxy/sozu#1380's own proposal: below it, `body_size` and `req_id` flood
# the candidate set.
TEST_NAME = re.compile(r"`([a-z][a-z0-9_]{12,})`")
# A sentence, not a noun phrase: five underscore-separated segments. A
# configuration key or a struct field stops at four.
MIN_NAME_SEGMENTS = 5
# The enclosing prose must be talking about tests for a name in it to be read
# as a test citation.
TEST_WORD = re.compile(r"\btests?\b", re.IGNORECASE)
# `fn name`, with line comments stripped first so that prose *describing* a
# function cannot vouch for a citation. Stripping can only SHRINK the set of
# known names, which is the safe direction: it produces a report to
# disposition, never a silent pass.
FN_DECL = re.compile(r"\bfn\s+([A-Za-z_][A-Za-z0-9_]*)")
LINE_COMMENT = re.compile(r"//.*$", re.MULTILINE)

# A test deliberately cited by a name it no longer carries, mapped to the name
# it carries now. Each of these is prose that says so in the same breath — "it
# is now `X`", "this test is the INVERSION of `Y`" — kept so the behaviour
# change leaves a trace. The checker holds the forwarding pointer live: the
# value must itself name a `fn`, or the citation is reported.
RENAMED_TESTS = {
    # Renamed because the old name claimed an ordering the body could not
    # observe: it inspects the IoSlice vector only after `confirm` returns,
    # where clear-before-consume and clear-after-consume both leave it empty.
    # The CHANGELOG cites the old name on purpose, to record that the rename
    # is the fix.
    "confirm_clears_the_descriptors_before_consuming":
        "confirm_leaves_no_descriptor_for_the_next_round",
    # sozu#1356 inverted the test that pinned the hostname-segment anchoring
    # defect instead of deleting it, as that test's own comment asked.
    "an_alternating_regex_hostname_segment_is_still_anchored_at_one_end_only":
        "an_alternating_regex_hostname_segment_is_anchored_on_every_branch",
    # sozu#1350, same shape: #1352's test pinned the unanchored path-regex
    # behaviour so that anchoring it later would be deliberate. It was.
    "a_path_regex_is_unanchored_and_matches_anywhere_in_the_request_path":
        "a_path_regex_is_anchored_at_both_ends_and_must_match_the_whole_request_path",
    # Renamed because the old name claimed a guarantee its body did not make.
    "reactivating_a_udp_listener_cannot_overwrite_a_live_session":
        "a_deactivated_udp_listeners_key_is_not_handed_to_another_session",
    # Re-based on one UDP frontend per address, which made the old premise
    # impossible rather than merely untested.
    "remove_udp_frontend_spares_same_address_siblings":
        "remove_udp_frontend_drops_exactly_the_frontend_its_tags_name",
    # sozu#1456, the same shape as the two regex entries above: this test
    # pinned the connection-global round-robin cursor as OBSERVED behaviour,
    # and its own doc comment asked for it to be deleted rather than
    # re-expected when per-bucket rotation landed. It was. The CHANGELOG
    # entries that describe the commits before it cite the old name on
    # purpose, to record that the deletion is the fix; the scenario — the
    # same four streams over the same six passes — is now this test, with the
    # corrected oracle.
    "the_round_robin_cursor_is_connection_global_so_only_the_leading_bucket_rotates":
        "every_urgency_bucket_rotates_its_own_incremental_tail",
}

# Sentence-shaped identifiers that survive both filters and are not test names.
# Every entry carries the reason it is here; an entry without one is an
# unreviewed silencing of this rule.
NOT_A_TEST = {
    "h2_graceful_shutdown_deadline_seconds": "listener configuration key",
    "h2_max_rst_stream_per_window": "listener configuration key",
    "h2_max_header_list_size": "listener configuration key",
    "h2_max_header_table_size": "listener configuration key",
    "select_nth_unstable_by_key": "std library method",
    "total_abusive_rst_received_lifetime": "H2FloodDetector field name, not a test",
}


def fn_names(root):
    """Every `fn <name>` declared in the tree, line comments stripped first."""
    names = set()
    for dirpath, dirnames, filenames in os.walk(root):
        dirnames[:] = [d for d in dirnames if d not in SKIP_DIRS]
        for name in filenames:
            if not name.endswith(".rs"):
                continue
            with open(os.path.join(dirpath, name), encoding="utf-8", errors="replace") as handle:
                body = LINE_COMMENT.sub("", handle.read())
            names.update(match.group(1) for match in FN_DECL.finditer(body))
    return names


def prose_files(root):
    """The surface rule 3 reads: every `*.rs`, plus CHANGELOG.md and the docs.

    A test citation lives wherever a claim does — a module `//!` preamble, a
    `///` doc comment, a `//` note inside a test body, a CHANGELOG entry. All
    four carried one of sozu-proxy/sozu#1380's six.
    """
    found = []
    for dirpath, dirnames, filenames in os.walk(root):
        dirnames[:] = [d for d in dirnames if d not in SKIP_DIRS]
        for name in filenames:
            rel = os.path.relpath(os.path.join(dirpath, name), root).replace(os.sep, "/")
            if name.endswith(".rs"):
                found.append((rel, True))
            elif rel == "CHANGELOG.md" or name == "LIFECYCLE.md" or (
                rel.startswith("doc/") and name.endswith(".md")
            ):
                found.append((rel, False))
    return sorted(found)


def prose_blocks(text, is_rust):
    """Split a file into prose blocks, each `(text, first line number)`.

    A block is the unit the "is this prose about tests?" question is asked of:
    a contiguous run of comment lines in Rust, a blank-line-delimited paragraph
    in markdown. Anything wider would let one distant mention of the word vouch
    for a whole file.
    """
    blocks = []
    current = []
    start = None
    for number, line in enumerate(text.splitlines(), 1):
        stripped = line.lstrip()
        keep = stripped.startswith("//") if is_rust else bool(line.strip())
        if keep:
            if start is None:
                start = number
            current.append(stripped)
        elif current:
            blocks.append(("\n".join(current), start))
            current = []
            start = None
    if current:
        blocks.append(("\n".join(current), start))
    return blocks


def check_test_citations(root, renamed=None, not_a_test=None, show=False, out=sys.stdout):
    """Every test name cited in prose must name a `fn` in the tree.

    Returns `(examined, checked, failures)`: how many backticked identifiers
    were long enough to be candidates at all, how many survived both filters
    and were actually resolved, and the failures among those.
    """
    renamed = RENAMED_TESTS if renamed is None else renamed
    not_a_test = NOT_A_TEST if not_a_test is None else not_a_test
    known = fn_names(root)
    examined = 0
    checked = 0
    failures = []

    for rel, is_rust in prose_files(root):
        with open(os.path.join(root, rel), encoding="utf-8", errors="replace") as handle:
            body = handle.read()
        for block, start in prose_blocks(body, is_rust):
            about_tests = bool(TEST_WORD.search(block))
            for match in TEST_NAME.finditer(block):
                examined += 1
                name = match.group(1)
                if name.count("_") + 1 < MIN_NAME_SEGMENTS or not about_tests:
                    continue
                checked += 1
                where = "%s:%d" % (rel, start)
                if name in not_a_test:
                    if show:
                        out.write("%s  `%s`  |not a test: %s\n" % (where, name, not_a_test[name]))
                    continue
                if name in known:
                    if show:
                        out.write("%s  `%s`  |names a fn in the tree\n" % (where, name))
                    continue
                if name in renamed:
                    target = renamed[name]
                    if target in known:
                        if show:
                            out.write("%s  `%s`  |renamed to `%s`\n" % (where, name, target))
                        continue
                    failures.append(
                        "%s: `%s` — recorded as renamed to `%s`, which names no `fn` in the "
                        "tree either; the forwarding pointer is dead too"
                        % (where, name, target)
                    )
                    continue
                failures.append(
                    "%s: `%s` — cited as a test, but names no `fn` in the tree" % (where, name)
                )

    return examined, checked, failures


# ── Rule 4: pinned snippets ───────────────────────────────────────────────
#
# See "STALE CODE QUOTED IN PROSE" in the header for the measurements that
# shaped this rule, and for the two alternatives it was chosen over.

# A fenced block whose info string carries, after the language, exactly one
# citation: ```rust lib/src/protocol/mux/hpack_state.rs:121-131
#
# The info string is the right place for it. CommonMark trims the text after
# the opening fence and calls it the info string, and every renderer in use
# takes only its FIRST word as the language — so the annotation highlights as
# Rust and stays invisible in the rendered page, while a citation written in
# the prose above would have to be read by a heuristic ("which paragraph
# belongs to which block?") that has no right answer.
#
# It is also, deliberately, an ordinary citation: rules 1 and 2 already see it
# because the fence line is part of the document body, so a pinned block gets
# range-checking immediately and drift-checking from the NEXT commit onward —
# rule 2 exempts a citation the base revision of the document did not carry —
# and this rule only adds the literal comparison on top.
#
# Both patterns match the INFO STRING — the text after the opening fence's
# backticks — never the whole line. The fence shape is `FENCE`'s business
# alone, so there is one place that knows how a fence is spelled.
PINNED_FENCE = re.compile(
    rf"^[A-Za-z0-9_+#-]*[ \t]+(?P<path>{PATH}):(?P<spans>{SPAN}(?:[,/][ \t]*{SPAN})*)[ \t]*$"
)
# The same shape without a citation — a plain ```rust — which this rule does
# NOT look at. Named so the count of unpinned Rust blocks can be reported
# beside the pinned ones: the rule's honest coverage number is that ratio.
RUST_FENCE = re.compile(r"^(?:rust|rs)[ \t]*$")

# A fence LINE: optional indentation, a run of three or more backticks, then
# the info string. The run's LENGTH is load-bearing and the `startswith("```")`
# scan that stood here was blind to it.
#
# CommonMark closes a fenced block only on a fence AT LEAST AS LONG as the one
# that opened it, carrying nothing after it but whitespace. That is the only
# way a document can SHOW an annotated fence without pinning it, and
# `doc/README.md` documents this very rule by doing exactly that. A scanner
# that closed on any ``` mistakes such an inner example for its enclosing
# block's closing fence and then runs one block OUT OF PHASE for the rest of
# the document.
#
# Measured on this repository's own fixtures before the fix: a document
# holding an unmatched inner opening fence took the pinned count from 3 to 2,
# left a deliberately stale pin unreported, and exited 0. A count that SHRINKS
# reads exactly like a pass, which is why `fenced_blocks` is worth getting
# right rather than approximating.
#
# Two limits, both deliberate. Tilde fences are not recognised: this tree has
# none, and PINNED_FENCE names backticks anyway. And an opening fence is
# accepted at any indentation rather than CommonMark's three columns — a fence
# this scanner cannot see is a fence it cannot PAIR, and an unpaired one is the
# phase error above, so erring toward seeing too many is the safe direction.
FENCE = re.compile(r"^[ \t]*(?P<ticks>`{3,})(?P<info>.*)$")


def fenced_blocks(body):
    """Every fenced block in a markdown body as `(info, lines, first_line)`.

    `info` is the info string — what follows the opening fence's backticks —
    with surrounding whitespace kept, since PINNED_FENCE anchors on it.

    `first_line` is the 1-based line of the OPENING fence, which is the line
    an annotation sits on and therefore the line a failure must name.

    Closing follows CommonMark: at least as many backticks as the opener, and
    nothing but whitespace after them. See `FENCE` for what that buys.
    """
    lines = body.splitlines()
    blocks = []
    index = 0
    while index < len(lines):
        opening = FENCE.match(lines[index])
        if opening is None:
            index += 1
            continue
        open_at = index
        ticks = len(opening.group("ticks"))
        index += 1
        while index < len(lines):
            closing = FENCE.match(lines[index])
            if (
                closing is not None
                and len(closing.group("ticks")) >= ticks
                and not closing.group("info").strip()
            ):
                break
            index += 1
        blocks.append((opening.group("info"), lines[open_at + 1 : index], open_at + 1))
        index += 1
    return blocks


def trim_blank_edges(lines):
    """Drop leading and trailing blank lines; keep the interior as it is.

    A quoted span routinely starts or ends on a blank line the doc author has
    no reason to reproduce, and a block routinely carries a blank line after
    the fence. Interior blanks are kept and compared, because a dropped line
    inside a quote changes what the quote says.
    """
    start, end = 0, len(lines)
    while start < end and not lines[start].strip():
        start += 1
    while end > start and not lines[end - 1].strip():
        end -= 1
    return lines[start:end]


def check_pinned_snippets(root, show=False, out=sys.stdout):
    """A fenced block that names the lines it quotes must quote them exactly.

    Returns `(pinned, unpinned, failures)`: how many fenced blocks carried an
    annotation and were therefore compared, how many Rust blocks carried none
    and were therefore not looked at, and the mismatches among the first.

    Comparison is on the STRIPPED line, matching rule 2, so the indentation a
    quote loses when it leaves an `impl` block is not a mismatch — but every
    other character is. There is no elision syntax on purpose: a block that
    cannot be quoted verbatim simply carries no annotation, which is what
    keeps this rule off the abbreviated and illustrative snippets that are the
    majority of the Rust blocks in this tree.

    The index is `target_files`, not a Rust-only one: `PINNED_FENCE` is built
    from the shared `PATH`, so a pin may name a `.md` target exactly as a prose
    citation may, and a narrower index here would silently skip such a block
    instead of checking it (an unresolvable path is rule 1's to report).
    """
    by_suffix = target_files(root)
    cache = {}
    failures = []
    pinned = 0
    unpinned = 0

    for doc in doc_files(root):
        doc_dir = os.path.dirname(doc)
        with open(os.path.join(root, doc), encoding="utf-8") as handle:
            body = handle.read()

        for info, block, fence_line in fenced_blocks(body):
            match = PINNED_FENCE.match(info)
            if match is None:
                if RUST_FENCE.match(info):
                    unpinned += 1
                continue

            cited = match.group("path")
            spans_text = " ".join(match.group("spans").split())
            spans = parse_spans(match.group("spans"))
            where = "%s:%d" % (doc, fence_line)

            target, _ = resolve_path(cited, by_suffix, root, doc_dir)
            if target is None:
                continue  # rule 1 reports an unresolvable path
            if target not in cache:
                with open(os.path.join(root, target), encoding="utf-8") as handle:
                    cache[target] = handle.read().splitlines()
            lines = cache[target]

            # Rule 1 owns every out-of-range and inverted span, and reports it
            # with its own wording; re-reporting it here would double every
            # such failure in the log.
            if any(start < 1 or start > end or end > len(lines) for start, end in spans):
                continue

            pinned += 1
            source = []
            numbers = []
            for start, end in spans:
                for number in range(start, end + 1):
                    source.append(lines[number - 1])
                    numbers.append(number)

            # Trim the source and the block independently, then re-derive the
            # source line numbers that survived the trim so a mismatch can name
            # the real line it is talking about.
            head = 0
            while head < len(source) and not source[head].strip():
                head += 1
            tail = len(source)
            while tail > head and not source[tail - 1].strip():
                tail -= 1
            source, numbers = source[head:tail], numbers[head:tail]
            quoted = trim_blank_edges(block)

            if len(quoted) != len(source):
                failures.append(
                    "%s: `%s:%s` — pinned block quotes %d line%s, %s:%s is %d"
                    % (
                        where,
                        cited,
                        spans_text,
                        len(quoted),
                        "" if len(quoted) == 1 else "s",
                        target,
                        spans_text,
                        len(source),
                    )
                )
                continue

            differing = [
                offset
                for offset in range(len(source))
                if quoted[offset].strip() != source[offset].strip()
            ]
            if not differing:
                if show:
                    out.write("%s  %s:%s  |%d lines quoted verbatim\n" % (where, target, spans_text, len(source)))
                continue

            first = differing[0]
            block_quoted, source_quoted = quote_pair(quoted[first], source[first])
            failures.append(
                "%s: `%s:%s` — pinned block line %d does not match %s:%d: block has `%s`, "
                "source has `%s`%s"
                % (
                    where,
                    cited,
                    spans_text,
                    first + 1,
                    target,
                    numbers[first],
                    block_quoted,
                    source_quoted,
                    "" if len(differing) == 1 else " (%d of %d lines differ)" % (len(differing), len(source)),
                )
            )

    return pinned, unpinned, failures


# ── Rule 5: line numbers cited from a Rust comment ──────────────────────
#
# See "LINE NUMBERS CITED FROM A RUST COMMENT" in the header for the
# measurements that shaped this rule, and for why it FORBIDS the form instead
# of resolving it the way rules 1 and 2 do for the documents.

# A citation is read from a line whose first non-blank characters are `//`.
# That is `prose_blocks`'s own definition of a Rust comment, kept identical on
# purpose: rules 3 and 5 must never disagree about what a comment is. `///` and
# `//!` begin with `//` and are therefore included, which matters — two of the
# sites this rule was written for are `//!` module preambles.
#
# THE GAP THIS LEAVES, named rather than implied: a TRAILING comment on a code
# line — `let n = 1; // see h2.rs:12` — is not read. Measured on the tree this
# rule landed in: all EIGHT `path.rs:NNN` tokens in Rust comments sat on a
# comment-leading line and none on a trailing one, so reading "everything after
# the first `//`" would have bought zero sites and cost the `//` inside a string
# literal, which is a match to this pattern and not a comment to the compiler
# (`"https://example.invalid/x.rs:1"`). Narrow and measured, like
# `BARE_CITATION`'s two discriminators. No site in the tree takes that branch
# today, and a citation moved onto one would be reported by nothing — which is
# the honest cost of the choice, not a reason to widen it blind.
RUST_COMMENT_LINE = "//"

# A citation a Rust comment may keep, with the reason it keeps it. The shape is
# NOT_A_TEST's, for NOT_A_TEST's reason: every entry carries the reason it is
# here, and an entry without one is an unreviewed silencing of this rule. The
# key is the citing FILE and the exact citation text, so an entry exempts one
# written site and never a form.
#
# The class is a citation that records a position at a revision that is GONE.
# Renumbering one onto today's tree does not repair it, it falsifies a record;
# converting it to a symbol cannot be done either, because the construct it
# names was deleted. So such a citation is neither converted nor renumbered —
# it is declared here and left exactly as its author wrote it.
#
# This table is NOT the place for a citation that is merely inconvenient to
# convert. A live pointer into a file this repository still edits has a symbol
# to name, and naming it is the whole remedy this rule exists to force.
HISTORICAL_CITATIONS = {
    ("lib/src/protocol/mux/h2_flood_detector.rs", "h2.rs:6946"):
        "a position AT THE PRE-EXTRACTION REVISION, which the citing sentence "
        "says out loud. It records where `H2FloodDetector::default()`'s test "
        "call sites were before this module took them — not where anything is "
        "now. That extraction deleted the `impl Default` the number describes, "
        "so there is no symbol to cite instead and no line to renumber onto",
}


def rust_files(root):
    """Every `*.rs` in the tree, repo-root-relative, `SKIP_DIRS` excluded.

    THE WHOLE TREE, not `lib|command|bin|e2e/src` alone. The defect is a
    property of the form, not of the directory the comment happens to sit in,
    and scoping by the citing path would leave `sim/`, `fuzz/`, `lib/benches`,
    `lib/examples`, the integration tests and every `build.rs` free to
    reintroduce it — while reading as a rule that covers Rust comments.
    """
    found = []
    for dirpath, dirnames, filenames in os.walk(root):
        dirnames[:] = [d for d in dirnames if d not in SKIP_DIRS]
        for name in filenames:
            if name.endswith(".rs"):
                found.append(
                    os.path.relpath(os.path.join(dirpath, name), root).replace(os.sep, "/")
                )
    return sorted(found)


def check_rust_comment_citations(root, historical=None, show=False, out=sys.stdout):
    """No `//`, `///` or `//!` comment may cite a line number in this tree.

    Returns `(examined, external, exempt, failures)`: how many citations were
    found on comment lines at all, how many named no file in this tree, how
    many `historical` declares, and the failures among the rest.

    WHAT DECIDES A HIT is `resolve_path` — rule 1's own resolver, so this rule
    and the resolver can never disagree about which file a citation names. A
    citation that resolves, and one whose basename is AMBIGUOUS in this tree,
    both fail: each names a line this repository's own edits move. A citation
    that resolves to nothing here is skipped and counted, because a pinned
    reference into an external dependency is not moved by anything in this
    repository and this rule is not its owner — `doc/README.md` reached the
    same verdict on the two `kawa` citations when the convention landed.

    Resolution is sibling-first, exactly as for a module `LIFECYCLE.md`, which
    is what brings a bare `h1.rs:361-368` written from `mux/h2.rs` inside the
    rule. A finder grep keyed on a `lib/src/`-style prefix walks past that
    form, and a rule built on the same prefix would have shipped green over it.
    """
    historical = HISTORICAL_CITATIONS if historical is None else historical
    by_suffix = target_files(root)
    examined = 0
    external = 0
    exempt = 0
    failures = []

    for rel in rust_files(root):
        own_dir = os.path.dirname(rel)
        with open(os.path.join(root, rel), encoding="utf-8", errors="replace") as handle:
            body = handle.read()
        for number, line in enumerate(body.splitlines(), 1):
            if not line.lstrip().startswith(RUST_COMMENT_LINE):
                continue
            for match in CITATION.finditer(line):
                examined += 1
                cited = match.group("path")
                text = "%s:%s" % (cited, " ".join(match.group("spans").split()))
                where = "%s:%d" % (rel, number)
                target, why = resolve_path(cited, by_suffix, root, own_dir)
                if target is None and why == "no such file in the tree":
                    external += 1
                    if show:
                        out.write("%s  `%s`  |external: names no file in this tree\n" % (where, text))
                    continue
                reason = historical.get((rel, text))
                if reason is not None:
                    exempt += 1
                    if show:
                        out.write("%s  `%s`  |historical: %s\n" % (where, text, reason))
                    continue
                failures.append(
                    "%s: `%s` — a line number cited from a Rust comment, naming %s; cite the "
                    "SYMBOL instead (`Type::method`), with the path and no line number"
                    % (where, text, target if target else "several files by that basename")
                )

    return examined, external, exempt, failures


# ── The audit mode: a citation that was ALREADY wrong ────────────────────
#
# See "AUDITING A CITATION THAT NEVER CHANGED" in the header for what this
# mode is for and why it never gates. Everything below is heuristic: it reads
# the citing PROSE and the cited LINE and asks whether they plausibly describe
# the same thing. That question has no mechanical answer, so this mode reports
# and never fails, and every constant here carries the measurement that chose
# it. Two trees are measured throughout — `6172929e`, the revision this mode
# landed on, and `5d5191e8`, the revision sozu-proxy/sozu#1466 filed its four
# wrong citations against, which is the only tree where the answers are known.

# The statement nouns sozu-proxy/sozu#1466's own heuristic names, and no
# others. Each says the prose is talking about a thing that EXECUTES — an
# insert, a push, a call — which is what makes a comment line the wrong target
# for it.
#
# "branch", "path" and "loop" are deliberately NOT here although they read as
# belonging: this file's own convention text says "keep a line or a range only
# where the prose means a specific branch", and `doc/**` uses "path" for a code
# path and a filesystem path interchangeably. Measured: adding those three
# moves the number of citations this mode EXAMINES from 108 to 118 at
# `6172929e` and from 113 to 130 at `5d5191e8`, and moves the number it REPORTS
# not at all — 13 and 15 either way. Nothing in either tree argues for them,
# and a noun list is a false-positive multiplier the day one does.
AUDIT_STATEMENT_NOUN = re.compile(
    r"\b(?:statement|statements|call|calls|insert|inserts|push|pushes"
    r"|arm|arms|field|fields|assignment|assignments)\b",
    re.IGNORECASE,
)

# A code span in the prose. The audit reads only BACKTICKED text, never bare
# prose words: these documents write every symbol in backticks, and reading
# unbacked words turns `Resume` the English verb into `H2WritePhase::Resume`.
BACKTICKED = re.compile(r"`([^`\n]+)`")

# What a backticked span has to look like to be read as a SYMBOL: identifier
# segments joined by `::` or `.`, with an optional call suffix.
# `saturating_sub(1)` and `self.stream_table.rst_sent_contains(sid)` both
# match; `gauge_add!`, a metric name with a hyphen and a markdown table row do
# not.
AUDIT_SYMBOL = re.compile(
    r"^[A-Za-z_][A-Za-z0-9_]*(?:\s*(?:::|\.)\s*[A-Za-z_][A-Za-z0-9_]*)*(?:\(.*\))?$"
)

# A name shorter than this is not looked for in the target file. The signal is
# "the prose names something that is not there", and a short segment is not a
# name — `get`, `now`, `push` and `close` each occur dozens of times in one
# `h2.rs`, so searching for them answers "present" everywhere, which is the
# same as not running. Measured at `6172929e`: 4 reports 19 findings, 6 reports
# 16, 8 reports 13, 12 reports 9. 8 is where the generic names stop entering
# and `rst_sent` — sozu-proxy/sozu#1466's own prose — is still readable.
AUDIT_MIN_SEGMENT = 8

# How far from the cited line the named symbol may sit and still count as
# present. Measured at `6172929e`: 3 reports 14 findings, 8 reports 13, 16
# reports 8, 32 reports 6. Widening it past 8 is pure suppression: the five
# findings that disappear between 8 and 16 were hand-audited, and two of them —
# `mux/LIFECYCLE.md`'s `mod.rs:2382-2383` and `mod.rs:2383`, both naming
# `Mux::shutting_down` while pointing into `shutting_down_inner` — are wrong
# citations repaired by this changeset. Nor is the window what makes the signal
# work, which is the other half of the argument: #1466's `h2.rs:710`
# names `rst_sent_contains`, whose nearest occurrence is over 1800 lines away.
# The window exists to forgive a citation pointing a few lines inside what the
# prose names, and 8 is enough for that.
AUDIT_WINDOW = 8

# A path written in backticks (`mod.rs`) matches AUDIT_SYMBOL — a `.` joins two
# identifier segments — and is not a symbol. Tested on the SEGMENT so that
# `doc/README.md` and a bare `h2.rs` are both caught.
AUDIT_PATH_SEGMENTS = {"rs", "md"}

# The item declarations a cited line can sit INSIDE. Kept to what a single line
# states, because this file parses no Rust and is not going to start.
ENCLOSING_FN = re.compile(r"\bfn\s+([A-Za-z_][A-Za-z0-9_]*)")
ENCLOSING_TYPE = re.compile(
    r"\b(?:struct|enum|trait|union)\s+([A-Za-z_][A-Za-z0-9_]*)"
    r"|\bimpl\b.*?\bfor\s+([A-Za-z_][A-Za-z0-9_]*)"
    r"|\bimpl(?:\s*<[^>]*>)?\s+([A-Za-z_][A-Za-z0-9_]*)"
)

# The prose fragment is quoted whole up to this width. Wider than QUOTE_WIDTH's
# 72 on purpose: rule 2 quotes ONE line of code to show what moved, while a
# finding here has to carry enough of a sentence for a human to decide whether
# it is about the line it cites.
AUDIT_PROSE_WIDTH = 140


def audit_fragment(body, doc_line, offset, line_starts):
    """The prose a citation is embedded in, and where in it the citation sits.

    Returns `(fragment, offset_into_fragment)`: the citation's own document
    line, plus the line ABOVE it when the citation is the first thing on its
    own line. A citation in that position is a wrapped continuation of the
    sentence above, and reading its own line alone reads nothing.

    That is not an edge case, it is the difference between catching
    sozu-proxy/sozu#1466's fourth citation and not: at `5d5191e8`,
    `lib/src/protocol/mux/LIFECYCLE.md` put `h2.rs:2775` alone on its line
    under the prose naming `completed_streams.push`. Measured on that tree,
    own-line-only reads 85 citations instead of 113, reports 11 findings
    instead of 15, and reports THREE of #1466's four instead of four.

    "First thing on its line" is decided on ALPHANUMERICS, not on whitespace: a
    wrapped citation is routinely preceded by `(`, `- ` or a list marker, and a
    whitespace-only test would take the line above for none of them.
    """
    line = doc_line(offset)
    start = line_starts[line - 1]
    before = body[start:offset]
    text = body[start:line_starts[line]] if line < len(line_starts) else body[start:]
    own = text.strip()
    here = len(" ".join(before.split()))
    if any(ch.isalnum() for ch in before) or line < 2:
        return own, here
    above = " ".join(body[line_starts[line - 2]:start].split())
    if not above:
        return own, here
    return (above + " " + own).strip(), len(above) + 1 + here


def audit_symbol(fragment, here):
    """The one symbol THIS citation is attached to, or None.

    THE NEAREST ONE BEFORE IT, not every symbol in the fragment. A document
    line routinely carries two citations and three symbols, and reading them
    all asks a citation to account for a name belonging to the clause beside
    it. Measured at `6172929e`: reading the first symbol anywhere on the
    fragment reports 24 findings, reading the attached one reports 13, and all
    eleven that disappear were hand-audited and are correct citations. Every
    one of them is this shape —

        Successful parse (`expect.rs:219-236`) stores the `ProxyAddr` into
        `self.addresses` …

    where the symbol is the sentence's object, further down the span, and the
    citation lands exactly where it says it does. The narrower reading still
    reports all four of sozu-proxy/sozu#1466's citations, so it costs nothing
    that is known to be a defect.

    THE LAST SEGMENT of that symbol, and only when it is at least
    AUDIT_MIN_SEGMENT long. Not the longest segment, which reads as the safer
    choice and is the opposite: a citation almost always points INSIDE the item
    it names, and a type name does not occur inside its own impl body, so
    keyed on the longest segment `ConnectionH2::reset_stream` looks for
    `ConnectionH2` at a line in the middle of `reset_stream` and reports a
    correct citation. Measured at `6172929e`: longest-segment reports 16,
    last-segment reports 13.

    A token whose last segment is SHORTER than that — `HttpAnswers::get`,
    `ParsingPhase::Error`, `completed_streams.push` — yields nothing rather
    than being walked back to its type: the prose named a member, so the type
    is the wrong thing to look for. That cost is paid on #1466's own fourth
    citation, whose prose is `completed_streams.push` exactly; this signal
    cannot test it and the comment signal is what catches it.

    A citation's own text is skipped, and a backticked bare PATH ends the
    search rather than being stepped over: `unlink_stream` (`mod.rs`) is this
    repository's symbol-citation form, so a path in backticks is another
    citation, and walking past one reads ITS symbol against this one's line.
    """
    for match in reversed(list(BACKTICKED.finditer(fragment))):
        if match.end() > here:
            continue
        token = match.group(1).strip()
        if CITATION.search(token) or not AUDIT_SYMBOL.match(token):
            continue
        segments = [s.strip() for s in re.split(r"::|\.", token.split("(", 1)[0]) if s.strip()]
        if any(s in AUDIT_PATH_SEGMENTS for s in segments):
            return None
        last = segments[-1]
        if last == "self" or len(last) < AUDIT_MIN_SEGMENT:
            return None
        return last
    return None


def audit_names_construct(fragment):
    """Does this prose name something that EXECUTES, rather than an item?

    Two ways, both narrow. A statement noun from AUDIT_STATEMENT_NOUN, or a
    backticked symbol carrying a call, a field access or a path separator —
    `completed_streams.push`, `saturating_sub(1)`, `Type::method`. A bare
    `Prioriser` names an item and is not a construct: a citation pointing at
    the doc comment ABOVE an item is what a reader wants, so naming one must
    not arm the comment signal.
    """
    if AUDIT_STATEMENT_NOUN.search(fragment):
        return True
    for text in BACKTICKED.findall(fragment):
        token = text.strip()
        if CITATION.search(token) or not AUDIT_SYMBOL.match(token):
            continue
        segments = [s.strip() for s in re.split(r"::|\.", token.split("(", 1)[0]) if s.strip()]
        if any(s in AUDIT_PATH_SEGMENTS for s in segments):
            continue
        if len(segments) > 1 or "(" in token:
            return True
    return False


def comment_kind(line):
    """`//!`, `///` or `//` for a comment line; None for anything else.

    The three are reported separately because they are not the same claim. A
    `//!` is a module preamble and a `///` documents the item below it, so a
    citation landing on one may well be pointing at exactly what the prose
    means; a plain `//` is a remark about the code beside it.
    """
    stripped = line.lstrip()
    for marker in ("//!", "///", "//"):
        if stripped.startswith(marker):
            return marker
    return None


def enclosing_names(lines, start):
    """The nearest `fn` and the nearest type-ish item declared above `start`.

    This is the difference between a signal and a noise generator. The standing
    form in this tree is prose that names the ENCLOSING function and cites one
    statement inside it — `lib/src/protocol/mux/LIFECYCLE.md`'s list of
    `remove_dead_stream` call sites labels each entry with the function that
    holds it. The function name is nowhere near the cited line, because it is
    at the top of the function, so a pure proximity search calls every entry in
    that list wrong. Measured at `6172929e`: proximity alone reports 29
    findings, proximity plus this reports 13, and all 16 that disappear are
    prose naming the item its citation points inside.

    NEAREST PRECEDING, not "the item this line is really in" — nothing here
    parses Rust, so a line between two functions takes the name of the one
    above it. That direction is deliberate: it can only SILENCE a finding,
    never invent one, and this mode reports to a human.
    """
    names = set()
    seen_fn = False
    seen_type = False
    for number in range(min(start, len(lines)) - 1, -1, -1):
        line = lines[number]
        if not seen_fn:
            match = ENCLOSING_FN.search(line)
            if match:
                names.add(match.group(1))
                seen_fn = True
        if not seen_type:
            match = ENCLOSING_TYPE.search(line)
            if match:
                names.add(next(group for group in match.groups() if group))
                seen_type = True
        if seen_fn and seen_type:
            break
    return names


def _symbol_present(lines, start, end, symbol):
    """Is `symbol` near the cited span, or does it name an item enclosing it?"""
    if symbol in enclosing_names(lines, start):
        return True
    lo = max(0, start - 1 - AUDIT_WINDOW)
    hi = min(len(lines), end + AUDIT_WINDOW)
    # CASE-INSENSITIVELY, which is not laxity: this tree names a metric in
    # prose by its emitted string (`accept_queue.backpressure`) and in code by
    # the SCREAMING_CASE constant carrying it (`names::accept_queue::BACKPRESSURE`).
    # Measured: a case-sensitive search moves `6172929e` from 13 findings to 15
    # and `5d5191e8` from 15 to 17, and the additions are that one pair of
    # citations, each landing exactly on the line it names.
    pattern = re.compile(r"\b%s\b" % re.escape(symbol), re.IGNORECASE)
    return any(pattern.search(line) for line in lines[lo:hi])


def _clip_audit(text, width=QUOTE_WIDTH):
    """One line, whitespace collapsed, clipped with an ellipsis."""
    flat = " ".join(text.split())
    return flat if len(flat) <= width else flat[: width - 1] + "…"


def audit(root, show=False, out=sys.stdout):
    """Read every citation's PROSE against its cited LINE. Advisory, never a gate.

    Returns `(examined, declined, findings)`: how many cited spans were put to
    the heuristics at all, a sorted `(reason, count)` list of everything it
    declined to check, and the findings.

    Rule 2 compares a citation between two revisions, so a citation that was
    already wrong the first time it was seen is exempt forever — "unchanged" is
    exactly the condition for being exempt. This mode asks a different
    question, of one revision, and therefore has no such blind spot. What it
    has instead is a heuristic answer, which is why it reports and never fails.

    TWO SIGNALS, both measured against sozu-proxy/sozu#1466's four known-wrong
    citations at `5d5191e8`, which is the only tree where the answers are known:

      * THE TARGET IS A COMMENT while the prose names a construct that
        executes. Three of the four — `h2.rs:2567`, `:2668`, `:2775` — land on
        comment lines under prose naming an insert and a push.
      * THE PROSE NAMES A SYMBOL THAT IS NOT THERE, neither within
        AUDIT_WINDOW lines of the cited span nor as an item enclosing it. The
        fourth, `h2.rs:710`, cited as `self.stream_table.rst_sent_contains(sid)`,
        lands on `let total_before = *total;` — ordinary code, invisible to the
        first signal, with the nearest `rst_sent_contains` over 1800 lines away.

    A span that arms both is ONE finding carrying both signals, never two.

    THE COMMENT SIGNAL READS A SINGLE-LINE CITATION ONLY. A RANGE that starts
    on a comment is this tree's normal way of covering a branch together with
    the comment introducing it — `manager.rs:248-252` is the over-size check
    and the sentence above it — and treating a range like a single line turns
    that convention into a report. Measured at `6172929e`: reading ranges too
    takes this mode from 13 findings to 26, of which 15 are comment findings
    and 13 of those 15 are ranges; all 13 were hand-audited and all 13 are
    correct citations. All three of #1466's comment-line citations are single
    lines, so the restriction costs nothing that is known to be a defect.
    """
    by_suffix = target_files(root)
    cache = {}
    findings = []
    examined = 0
    declined = {}

    def decline(reason):
        declined[reason] = declined.get(reason, 0) + 1

    for doc in doc_files(root):
        doc_dir = os.path.dirname(doc)
        with open(os.path.join(root, doc), encoding="utf-8") as handle:
            body = handle.read()
        doc_line = doc_line_finder(body)
        line_starts = [0]
        for text in body.splitlines(keepends=True):
            line_starts.append(line_starts[-1] + len(text))

        for cited, spans_text, spans, offset in citations(body):
            where = "%s:%d" % (doc, doc_line(offset))
            if cited is None:
                decline("a bare continuation binding to no path (rule 1 reports it)")
                continue
            target, _ = resolve_path(cited, by_suffix, root, doc_dir)
            if target is None:
                decline("a cited path that does not resolve (rule 1 reports it)")
                continue
            if not target.endswith(".rs"):
                decline("a prose-to-prose citation, which has no code shape to read")
                continue
            if target not in cache:
                with open(os.path.join(root, target), encoding="utf-8") as handle:
                    cache[target] = handle.read().splitlines()
            lines = cache[target]

            fragment, here = audit_fragment(body, doc_line, offset, line_starts)
            names_construct = audit_names_construct(fragment)
            symbol = audit_symbol(fragment, here)

            for start, end in spans:
                span = str(start) if start == end else "%d-%d" % (start, end)
                if start < 1 or start > end or end > len(lines) or not lines[start - 1].strip():
                    decline("a span rule 1 already rejects (out of range, inverted or blank)")
                    continue
                if not names_construct and symbol is None:
                    decline("prose naming nothing this heuristic can test")
                    continue
                examined += 1

                signals = []
                kind = comment_kind(lines[start - 1]) if start == end else None
                if kind is not None and names_construct:
                    signals.append(
                        "the target is a `%s` comment while the prose names a statement or "
                        "call — %s:%d is `%s`"
                        % (kind, target, start, _clip_audit(lines[start - 1]))
                    )
                if symbol is not None and not _symbol_present(lines, start, end, symbol):
                    signals.append(
                        "the prose names `%s`, which occurs nowhere within %d lines of %s:%s "
                        "and names no item enclosing it — %s:%d is `%s`"
                        % (symbol, AUDIT_WINDOW, target, span, target, start,
                           _clip_audit(lines[start - 1]))
                    )
                if not signals:
                    if show:
                        out.write(
                            "%s  %s:%s  |plausible: %s\n"
                            % (where, target, span, _clip_audit(lines[start - 1]))
                        )
                    continue
                findings.append(
                    "%s: `%s:%s`\n      prose: %s\n      %s"
                    % (where, cited, span, _clip_audit(fragment, AUDIT_PROSE_WIDTH),
                       "\n      ".join(signals))
                )

    return examined, sorted(declined.items()), findings


def audit_report(root, show=False):
    """Print the audit and ALWAYS return 0. See `audit` for the heuristics.

    Separate from `main`'s five-rule run rather than appended to it, and
    exiting 0 whatever it finds. A heuristic that fails a build is a heuristic
    people learn to silence, and the silencing outlives the reason — which is
    how a rule ends up with an exemption table nobody reads. This one produces
    a list a human dispositions, and the disposition is the deliverable.
    """
    examined, declined, findings = audit(root, show=show)
    print("ADVISORY: the citation audit is not a gate. It always exits 0, it never edits a")
    print("citation, and it is deliberately absent from the `Doc citations` CI job. Every")
    print("finding below is a GUESS that a human has to disposition against the code.")
    print("Measured on its own first run, at main `6172929e`: 13 findings, 6 of them wrong")
    print("citations and 7 false positives. Expect roughly half of what you read here to be")
    print("a citation that is doing its job.")
    print("")
    if findings:
        print(
            "%d of %d examined citations do not plainly match the prose citing them:"
            % (len(findings), examined)
        )
        for line in findings:
            print("  " + line)
            print("")
    else:
        print("No finding: all %d examined citations plainly match the prose citing them." % examined)
        print("That is not a proof. Both heuristics are crude, and a tree with no finding is")
        print("more likely to mean the prose says nothing testable than that every citation is")
        print("right — read the declined counts below before reading this as clean.")
        print("")

    print("What it DECLINED to check, beside what it checked:")
    if declined:
        for reason, count in sorted(declined, key=lambda pair: -pair[1]):
            print("  %5d  %s" % (count, reason))
    else:
        print("      0  nothing was declined")
    print("")
    print("Silence about skipped work is the defect that produced sozu-proxy/sozu#1457 and")
    print("sozu-proxy/sozu#1447, so the declined counts are printed whether or not anything")
    print("was found. The largest is prose that names nothing this heuristic can test: a")
    print("sentence with no backticked symbol and none of the statement nouns is read by")
    print("neither signal, and a wrong citation inside one is invisible here.")
    print("")
    print("Disposition each finding as `wrong`, `correct`, or `false positive` WITH the")
    print("reason — a false positive is information about the heuristic, not noise to")
    print("ignore. Where a finding is wrong and the prose names an item, cite the SYMBOL")
    print("and drop the number: a symbol cannot drift and needs no audit.")
    print("Convention and local usage: doc/README.md#auditing-citations-that-never-change")
    return 0


FIXTURE_EXPECTED = [
    "doc/bad.md:3: `sample.rs:99` — past end of sample.rs (10 lines)",
    "doc/bad.md:5: `sample.rs:4` — sample.rs:4 is blank",
    "doc/bad.md:7: `nowhere.rs:1` — no such file in the tree",
    "doc/bad.md:9: `sample.rs:3/3` — line 3 repeated inside one citation",
    "doc/bad.md:9: `sample.rs:99` — past end of sample.rs (10 lines)",
    "doc/bad.md:11: `sample.rs:0` — line numbers start at 1",
    "doc/bad.md:13: `sample.rs:3-5` — sample.rs:5 is blank (end of range)",
    "doc/bad.md:15: `h2.rs:99` — past end of h2.rs (3 lines)",
    "doc/bad.md:17: `reference.md:99` — past end of doc/reference.md (12 lines)",
    "doc/bad.md:19: `doc/reference.md:10` — doc/reference.md:10 is blank",
    "doc/bad.md:22: `sample.rs:99` — past end of sample.rs (10 lines)",
    "doc/bad.md:24: `sample.rs:3/3` — line 3 repeated inside one citation",
    "doc/bad.md:26: `:1/2` — a bare continuation with no citation earlier on its own line",
    "doc/bad.md:30: `:7` — a bare continuation with no citation earlier on its own line",
    "doc/bad.md:33: `:7` — a bare continuation with no citation earlier on its own line",
]

# The bare continuation on `doc/bad.md:9`, asserted ON ITS OWN and not only as
# an entry in the list above. It names line 99 of a 10-line file, so it is a
# failure of the plainest kind — and until sozu-proxy/sozu#1459 no rule in this
# file could report it, because `CITATION` requires a path and this citation
# carries none. It is extracted only by `citations()` binding it to the
# `sample.rs` earlier on its own line.
#
# The list assertion above cannot stand in for this one: when the binding is
# removed it prints "expected 11 failures, got 10" followed by the ten it DID
# find, which names every citation except the one that went missing. A count
# that shrinks is evidence something is gone; only this assertion says WHAT.
FIXTURE_BARE_EXPECTED = "doc/bad.md:9: `sample.rs:99` — past end of sample.rs (10 lines)"

# THE SCOPE, asserted rather than described. `citations()` inherits a path from
# the nearest written-out citation EARLIER ON THE SAME LINE, and each of the
# three ways that sentence can be weakened has a fixture whose verdict CHANGES
# under it. Measured, because the first version of this file asserted none of
# them and claimed otherwise: with only a blank-line-separated bare span to go
# on, widening the scope to the paragraph, or binding to the FIRST citation on
# the line instead of the nearest, both left `--self-test` at exit 0 — and on
# the real tree the paragraph widening produced byte-identical `--show` output,
# so nothing anywhere would have reported it.
#
#   * NEAREST, not first. `doc/bad.md:22` puts `h2.rs:1` and `sample.rs:3` on
#     one line and continues bare at `:99`. Binding to the nearest reports it
#     against `sample.rs` (10 lines); binding to the first reports `h2.rs`
#     (3 lines). Both are failures, so only the exact TEXT separates them —
#     which is why this one is a list entry above as well as a named constant.
FIXTURE_BARE_NEAREST_EXPECTED = "doc/bad.md:22: `sample.rs:99` — past end of sample.rs (10 lines)"

#   * SAME LINE, not the paragraph. `doc/bad.md:30` is a bare `:7` one line
#     below a `sample.rs:3` in its own paragraph. Same-line scope cannot bind
#     it and reports it; paragraph scope binds it to `sample.rs:7`, which is a
#     real non-blank line, so the failure DISAPPEARS and the run goes green
#     over a scope one paragraph wider than the header claims. `doc/bad.md:33`
#     is the same span with no citation in its paragraph at all, which is the
#     weaker document-wide case and the only one the first version caught.
FIXTURE_BARE_SAME_LINE_EXPECTED = "doc/bad.md:30: `:7` — a bare continuation with no citation earlier on its own line"

#   * THE GROUP FORM. `doc/bad.md:24` continues bare as `:3/3`, which is the
#     half-applied renumbering of a `/` group with the path dropped. It is
#     extracted only because `BARE_CITATION` carries `CITATION`'s span group;
#     with a single `{SPAN}` there it matches nothing at all, and the citation
#     goes back to being checked by no rule — #1459 one syntax step along.
FIXTURE_BARE_GROUP_EXPECTED = "doc/bad.md:24: `sample.rs:3/3` — line 3 repeated inside one citation"

# The exact number of citation groups the fixtures contain. Asserting the total
# — not a floor — is what makes a SHRINKING extraction surface fail the
# self-test, and each such shrink is one edit:
#   * dropping the digit from PATH loses every `h2.rs:N` citation. The fixtures
#     carry three, one of them an expected failure, so both this total and
#     FIXTURE_EXPECTED go wrong.
#   * dropping `**/LIFECYCLE.md` from doc_files() loses mod/LIFECYCLE.md's two.
#   * dropping `md` from PATH loses every markdown-target citation at once:
#     four clean in `doc/good.md` and two expected failures in `doc/bad.md`,
#     which is the exact shape sozu-proxy/sozu#1444 found in the tree — never
#     extracted, and therefore counted as nothing.
#   * dropping the bare-continuation binding from citations() loses eight at
#     once — `doc/bad.md`'s six and `doc/drift.md`'s `:13`, plus the group
#     `:1/2` — which is the exact shape sozu-proxy/sozu#1459 found in the tree:
#     a citation extracted by nothing, and therefore never reported as
#     unresolvable either.
#   * narrowing `BARE_CITATION` back to a single `{SPAN}` loses only the two
#     GROUP continuations, `doc/bad.md`'s `:3/3` and `:1/2`.
#     WIDENING the scope a bound continuation inherits over is the opposite
#     edit and this total cannot see it — every citation is still extracted,
#     just bound to a different path or bound where it should not be. That is
#     what FIXTURE_BARE_NEAREST_EXPECTED and FIXTURE_BARE_SAME_LINE_EXPECTED
#     are for, and neither is a count.
#   * dropping `.md` from TARGET_SUFFIXES loses only `doc/good.md`'s bare
#     `LIFECYCLE.md:8`, and that one citation is the whole reason it is there.
#     The other markdown fixtures resolve through resolve_path's first two
#     branches and never consult the suffix index at all, so before that
#     citation existed TARGET_SUFFIXES was assertable but unasserted: reverting
#     it alone left this self-test green. A constant no fixture can move is not
#     a guard.
# None of these is an accidental shape, and none announces itself: on the real
# tree they report a clean run over a quietly smaller surface.
# `doc/drift.md` contributes seven of these: rule 2's fixture is an ordinary
# document that rule 1 must also see, and see as clean. Six of the seven name
# `drift.rs` — one of them the bare `:13` continuing the range beside it — and
# the seventh names `doc/wide.md`, whose catalogue row is the drift whose text
# changes only PAST the width a quote is clipped at (#1448).
# `doc/pinned.md` and `doc/pinned_bad.md` contribute one each, for the same
# reason and with more force: rule 4's annotation lives in the document body,
# so rule 1 resolves it exactly as it resolves a citation written in prose —
# which is what lets a pinned block be range-checked without a second parser.
# `doc/pinned_nested.md` contributes two: the annotation it DISPLAYS inside a
# four-tick example is still a citation to rule 1, which is correct — the text
# names a real span either way — and the real pin after it is the second.
# `doc/keyed.md` contributes three, all naming `keyed.rs`, and they are rule 2's
# IDENTITY fixture: a two-span group with one span moved and one left behind
# (#1457), and a single span whose number named different code at the base
# revision (#1447). Rule 1 must pass all three at both revisions, or the
# document is testing the resolver instead of the comparison.
# `doc/bad.md` contributes seventeen, six of them bare continuations: two bound
# and wrong, one bound GROUP, one unbindable group, and two unbindable spans
# that pin the scope. `doc/good.md` contributes nine; `doc/reference.md`,
# `doc/wide.md` and `keyed.rs` none —
# each is a citation TARGET, and carries no citation of its own.
FIXTURE_TOTAL = 48

# Rule 2's half of the fixtures is a PAIR of revisions, so every file that
# drifts carries its base revision beside it as `<name>.base`. That suffix is
# what keeps those files invisible to all three surfaces — `drift.rs.base` is
# not `*.rs` and `doc/drift.md.base` is not `*.md`, so no walk in this script
# sees either — and they exist only for the self-test, which copies each over
# its live counterpart, commits that as the base, and restores the tree.
DRIFT_BASE_SUFFIX = ".base"

# `doc/drift.md` carries six citations and EVERY ONE of them resolves to a
# non-blank line at both revisions, so rule 1 is green on it in both
# directions and only the comparison separates the cases:
#   * `drift.rs:8` and `drift.rs:8-10` did not move   — must stay silent
#   * `drift.rs:12` moved onto another method's signature       — reported
#   * `drift.rs:8-13` kept its start and moved its end          — reported
#   * the bare `:13` beside it inherits `drift.rs` from that same
#     citation and moved with it                                 — reported,
#     which is what proves rule 2 reads a pathless continuation through
#     `citations()` exactly as rule 1 does, rather than only counting it
#   * `drift.rs:16` is the re-anchored form of the first drift  — must stay
#     silent, because it is absent from the base revision of the document
#   * `wide.md:11` moved only past the clip                     — reported,
#     and asserted apart from this list: see FIXTURE_DRIFT_WIDE_PREFIX
#
# `doc/keyed.md` adds the two the IDENTITY defects turn on, and they fail for
# opposite reasons — one was invisible, the other is reported and cannot be
# fixed:
#   * `keyed.rs:17` is the sibling of a two-span group whose FIRST span this
#     changeset moved (`keyed.rs:8/17` -> `keyed.rs:12/17`). Keyed on the whole
#     tuple the group was classified re-anchored and this span was never
#     compared, which is sozu-proxy/sozu#1457's false NEGATIVE — and the worse
#     half of it, because a group whose first element is freshly correct reads
#     as maintained. Seen red: with the per-span key reverted to
#     `tuple(spans)`, this entry disappears and the self-test prints
#     "expected 5 drifted citations, got 4".
#   * `keyed.rs:16` is sozu-proxy/sozu#1447's false positive, and it is NOT in
#     this list — it is pinned in FIXTURE_DRIFT_REUSED_EXPECTED below as a
#     declined comparison instead. The citation is CORRECT (the helper it names
#     really is on line 16 at HEAD) and 16 named a different helper at the base
#     revision, so a number-keyed exemption reported a citation no edit of the
#     document could fix. The text-keyed identity sees that the base's claim on
#     line 16 is cited again at `keyed.rs:20`, a number this changeset
#     introduced, and declines the span.
#
# `drift.rs:12` is what holds that identity honest FROM THE OTHER SIDE, and it
# is the reason this list must keep it. `doc/drift.md.base` cites `drift.rs:12`
# TWICE — once for the stale reading and once for the re-anchored one — and the
# head document re-anchors only one of the two, to `drift.rs:16`. One claim on
# `pub fn moved(&self) -> u8 {` is therefore unaccounted for, the receiver
# count does not reach the base claim count, and the span left behind is
# compared exactly as before. Drop the counting from `reanchored_claims` and
# this entry disappears: a reuse exemption widened until it swallowed the
# half-applied renumbering of sozu-proxy/sozu#1457 turns the self-test red.
FIXTURE_DRIFT_EXPECTED = [
    "doc/drift.md:11: `drift.rs:12` — drift.rs:12 moved: "
    "was `pub fn moved(&self) -> u8 {`, now `pub fn inserted(&self) -> u8 {`",
    "doc/drift.md:15: `drift.rs:13` — drift.rs:13 moved: "
    "was `1`, now `0`",
    "doc/drift.md:15: `drift.rs:8-13` — drift.rs:13 moved (end of range): "
    "was `1`, now `0`",
    "doc/keyed.md:7: `keyed.rs:17` — keyed.rs:17 moved: "
    "was `let far = 3;`, now `2`",
]

# The spans rule 2 DECLINED because this changeset reused their number, pinned
# by their exact reported text for the same reason FIXTURE_DRIFT_EXPECTED is:
# this is the one verdict here reached by comparing text across revisions
# rather than by a number's presence, so what the report SAYS is half of it. A
# reviewer disposition an exemption they cannot read, and a build that stopped
# naming the matched text or the number the claim moved to would still emit the
# same count.
#
# It is load-bearing in both directions. Revert `check_drift` to the
# number-keyed exemption and this list goes empty while `keyed.rs:16` reappears
# in FIXTURE_DRIFT_EXPECTED; widen `reanchored_claims` past its claim counting
# and `drift.rs:12` moves here out of FIXTURE_DRIFT_EXPECTED. Neither can be
# silenced without moving a fixture verdict.
FIXTURE_DRIFT_REUSED_EXPECTED = [
    "doc/keyed.md:16: `keyed.rs:16` — keyed.rs:16 not compared: the base named "
    "`pub fn tail(&self) -> u8 {` there, and this changeset re-anchored that line "
    "to keyed.rs:20",
]

# The sixth citation's drift is held OUT of the list above deliberately.
# `doc/wide.md:11` is a 151-character catalogue row whose only edit sits at
# character 147 — past QUOTE_WIDTH — which is the case sozu-proxy/sozu#1448
# reported from the real tree: clipped from column 0, the two revisions render
# to the SAME string, so the rule fires correctly and the report names nothing
# that moved. `doc/configure_admin_ops.md:158` cites exactly such a row
# (`doc/configure.md:1219`, 610 characters wide), and #1458 made that whole
# class reachable by teaching the resolver to read `.md` targets — the
# citations that point at prose disproportionately point at table rows.
#
# What this fixture asserts is a PROPERTY — the two quoted halves differ, and
# neither is longer than QUOTE_WIDTH — never their text. Pinning the text here
# would pin the window strategy, so the next person to change how the window is
# chosen would have to rewrite the assertion meant to be guarding them; and
# pinning a LENGTH ceiling is what makes "do not fix this by removing the clip"
# mechanical rather than advisory. The prefix names the citation, which is what
# separates this entry from the two whose text IS pinned.
FIXTURE_DRIFT_WIDE_PREFIX = "doc/drift.md:22: `wide.md:11` — doc/wide.md:11 moved: "

# The `was`/`now` halves of one drift report line. Both groups are greedy, so
# the inner backticks a markdown row carries — `knob` in this fixture — cannot
# end either one early: the separator `, now ` occurs once in the format string
# and the line ends with `now`'s closing backtick. A line this does NOT match
# is a self-test FAILURE and never a skip, because a report that changed shape
# is exactly when an assertion reading it would otherwise check nothing.
REPORTED_PAIR = re.compile(r": was `(.*)`, now `(.*)`$")

# The exact number of cited line ENDS compared across the clean fixture tree,
# asserted for the same reason FIXTURE_TOTAL is: a rule that quietly stopped
# comparing would otherwise report a clean run. Losing the document-directory
# binding, the range-end comparison or a whole fixture document each move it.
# Six of these are pin annotations, whose ranges have two ends like any other:
# `doc/pinned.md`'s one and `doc/pinned_nested.md`'s two. They are compared
# here because the fixture base revision already carries them — on the commit
# that first ADDS a pin, rule 2 exempts it (see "STALE CODE QUOTED IN PROSE").
# Seven more are `doc/good.md`'s markdown-target citations: rule 2 reads a
# `.md` target through the same `resolve_path`, so a markdown citation left
# behind by a changeset that moved its target is reported exactly as a Rust one
# is — which is what sozu-proxy/sozu#1437 re-anchored past unseen, and the
# durable half of #1444. Rule 1 can only say a markdown target exists; rule 2
# says its TEXT still reads the way the citing prose claims.
# The thirty-first is `doc/wide.md`'s catalogue row: one more markdown-target
# end, and the only one whose two revisions differ solely past the clip. The
# thirty-second is `doc/drift.md:15`'s bare `:13`: rule 2 resolves the path it
# inherited and compares it like any other, so a build of `citations()` that
# bound continuations for rule 1 alone would report a clean run over a surface
# one citation smaller than rule 1 just scanned.
#
# This constant is why sozu-proxy/sozu#1444 was rebased onto #1432 rather than
# merged. Both branches raised it from 17 to 23 — six pin-annotation ends there,
# six markdown-target ends here — so the ASSIGNMENT merged clean with no marker
# while the comment above it conflicted, leaving the wrong value one line below
# the `>>>>>>>` a resolver reads. Two correct edits, silently composed into a
# third value that is neither. When two branches move the same counter for
# different reasons, the merge is a sum, and git cannot know that.
# The thirty-third is `doc/keyed.md`'s sibling span, which a half-applied
# renumbering used to exempt — a span the WHOLE-TUPLE key skipped rather than
# decided, and the coverage the per-span key buys.
#
# `doc/keyed.md`'s reused number is deliberately NOT among these. It is
# declined, counted in FIXTURE_DRIFT_REUSED_EXPECTED, and this counter dropped
# by exactly one when the text-keyed identity landed. That direction matters:
# a reuse exemption is a comparison this rule no longer performs, so it has to
# show up as a smaller compared total and a non-empty declined list, never as a
# compared total that stayed the same.
FIXTURE_DRIFT_COMPARED = 38


# The exact number of cited line ENDS the rule declined to compare because this
# changeset re-anchored their span, and the reason it is asserted beside the
# compared total rather than left implicit: a compared count reports what the
# rule LOOKED AT and never what it declined to look at, so this rule's coverage
# can fall to zero for a whole class of work while every counter it emits stays
# healthy. Measured in sozu-proxy/sozu#1447 on a changeset that repaired six
# markdown citations: 370 cited ends compared with `.md` targets enabled, 370
# with them disabled, and ZERO of the six entered the rule — repairing a
# citation changes its span, which is exactly what the exemption covers. The
# run reported "none of the 370 cited lines changed their text" and was telling
# the truth about the 370 while saying nothing about the six.
#
# `compared` and `exempt` PARTITION every cited end that resolved and was in
# range AT HEAD, which is what makes the pair readable and what decides the
# order of the two range tests. An exempt span reads no base text, so only the
# HEAD range can bind it, and rule 1 already owns HEAD out-of-range; testing
# `min(base, head)` first instead would drop every citation renumbered into the
# GROWN TAIL of a file — which is not a corner, it is the dominant re-anchoring
# shape, since an insertion of any size pushes later citations past the old end
# by construction. `doc/drift.md:18`'s `drift.rs:16` is exactly that case: 16
# exceeds the base revision's 15 lines, and it is the third of these three.
# The base range is still checked, after the exemption, because a COMPARED span
# does need a line at the base to compare against.
FIXTURE_DRIFT_EXEMPT = 3

# Rule 3's half of the fixtures. `tests_bad.rs` and the fixture `CHANGELOG.md`
# are the broken documents; `tests_good.rs` is the clean one and also carries
# the two witnesses that must NOT be examined — a four-segment noun phrase in a
# block that does say "test", and a sentence-shaped name in a block that does
# not.
FIXTURE_TEST_EXPECTED = [
    "CHANGELOG.md:3: `a_fixture_changelog_test_name_that_names_no_function` — cited as a test",
    "tests_bad.rs:4: `a_fixture_test_that_was_renamed_or_never_landed` — cited as a test",
    "tests_bad.rs:9: `a_fixture_test_renamed_to_a_name_that_is_also_gone` — cited as a test",
]

# Asserting both totals — not floors — is what makes a quietly SHRINKING
# surface or a quietly TIGHTENED filter fail instead of reporting a clean run:
#   * dropping `*.rs` from prose_files() loses five of the six examined names;
#   * dropping CHANGELOG.md loses the sixth;
#   * raising MIN_NAME_SEGMENTS or narrowing TEST_WORD drops `checked` below 4
#     while every remaining citation still resolves.
FIXTURE_TEST_EXAMINED = 6
FIXTURE_TEST_CHECKED = 4

# Fixture-local tables, used to exercise the two dispositions on a tree where
# the real RENAMED_TESTS / NOT_A_TEST entries name nothing. `..._never_landed`
# is forwarded to a name `tests_good.rs` really declares, so it must pass;
# `..._also_gone` is forwarded to a name nothing declares, so it must be
# reported with the rename-specific message rather than silently accepted.
FIXTURE_RENAMED = {
    "a_fixture_test_that_was_renamed_or_never_landed":
        "a_fixture_test_cited_by_the_name_it_still_carries",
    "a_fixture_test_renamed_to_a_name_that_is_also_gone":
        "a_fixture_test_nothing_in_this_tree_declares",
}
FIXTURE_NOT_A_TEST = {
    "a_fixture_changelog_test_name_that_names_no_function": "fixture allowlist witness",
}

# Rule 4's half of the fixtures. `doc/pinned_bad.md` pins the SAME span as the
# clean `doc/pinned.md` and quotes it one rename out of date, so rule 1 resolves
# it, rule 2 finds it unmoved, and only the literal comparison separates the
# two — which is the whole claim this rule makes.
FIXTURE_PINNED_EXPECTED = [
    "doc/pinned_bad.md:8: `pinned.rs:11-15` — pinned block line 1 does not match pinned.rs:11: "
    "block has `pub fn shrink_buffers(&mut self) {`, source has `pub fn shrink(&mut self) {` "
    "(3 of 5 lines differ)",
]

# Asserting both totals — not floors — for the reason every other total here is
# asserted. `pinned` going to 0 is what a broken PINNED_FENCE looks like, and it
# would otherwise report a clean run over nothing; `unpinned` is the rule's own
# coverage number, so a fixture that silently stopped carrying an UNannotated
# Rust block would stop proving that such a block is left alone.
#
# `doc/pinned_nested.md` is the third pin, and it is here to hold the FENCE
# pairing specifically: it shows an annotated fence inside a four-tick block
# without pinning it, then carries a real pin after it. A scanner that closed
# the outer block on that inner opener runs out of phase and never sees the
# real pin — measured at 2 instead of 3, with a stale pin unreported and exit
# 0. Only an exact total turns that into a failure, because the count SHRINKS.
FIXTURE_PINNED = 3
FIXTURE_UNPINNED = 1

# The invariant half of rule 5's failure message, shared by every expectation
# below so that a reworded report is ONE edit here rather than six, and so
# that the expectations stay readable as what they actually discriminate:
# the citing site, the citation text, and the file it resolved to.
FIXTURE_COMMENT_MESSAGE = "— a line number cited from a Rust comment, naming"

# Rule 5's half of the fixtures. `comments_bad.rs` writes the forbidden form in
# each span shape `CITATION` carries — a single line, a range, and a `/` group
# — so a narrowed pattern loses a fixture instead of losing the tree quietly;
# `mod/comments_sibling_bad.rs` writes the BARE-SIBLING shape;
# `comments_good.rs` is the clean one and carries the two that must be left
# alone. The listed order is `sorted()`'s, which is lexicographic on the
# message and therefore on `file:line` as TEXT — `:11` before `:6`.
FIXTURE_COMMENT_EXPECTED = [
    "comments_bad.rs:11: `sample.rs:6-8` %s sample.rs" % FIXTURE_COMMENT_MESSAGE,
    "comments_bad.rs:12: `sample.rs:7/8` %s sample.rs" % FIXTURE_COMMENT_MESSAGE,
    "comments_bad.rs:16: `drift.rs:4` %s drift.rs" % FIXTURE_COMMENT_MESSAGE,
    "comments_bad.rs:6: `sample.rs:3` %s sample.rs" % FIXTURE_COMMENT_MESSAGE,
    "mod/comments_sibling_bad.rs:5: `h2.rs:2` %s mod/h2.rs" % FIXTURE_COMMENT_MESSAGE,
]

# THE SIBLING CITATION, asserted by name rather than only as a list entry.
# `h2.rs:2` written from `mod/` carries no directory, so the finder grep in
# sozu-proxy/sozu#1473 — keyed on a `lib/src/`-style prefix — walks past it,
# and a rule built on that same prefix would ship green over the live instance
# of exactly this shape: `mux/h2.rs`'s `h1.rs:361-368`, naming its own sibling.
# Only `resolve_path`'s directory-first binding reaches it. The assertion is on
# the RESOLVED TARGET, because that is the single character of the report that
# separates a correct binding from a plausible wrong one: root `h2.rs` is a
# real 3-line file and reports just as confidently.
FIXTURE_COMMENT_SIBLING_EXPECTED = (
    "mod/comments_sibling_bad.rs:5: `h2.rs:2` %s mod/h2.rs" % FIXTURE_COMMENT_MESSAGE
)

# Both totals asserted, not floors, for the reason every other total here is:
#   * dropping `*.rs` from rust_files(), or narrowing it to a directory list,
#     loses the sibling fixture and then the whole rule reports on less than it
#     claims to cover;
#   * requiring the citing line to start with `///` rather than `//` loses the
#     three plain-`//` header citations;
#   * dropping the external SKIP makes `external` 0 and turns `comments_good.rs`
#     into two failures, which is the rule claiming ownership of a dependency
#     this repository does not edit.
FIXTURE_COMMENT_EXAMINED = 7
FIXTURE_COMMENT_EXTERNAL = 2

# The fixture-local exemption table, used to exercise the disposition on a tree
# where the real HISTORICAL_CITATIONS entry names nothing. `comments_bad.rs`'s
# `drift.rs:4` is reported with the default table and silent with this one, so
# deleting this entry — or keying the table on anything but (file, citation) —
# moves a verdict and turns `--self-test` red. A table no fixture can move is
# not a guard; see TARGET_SUFFIXES for the same lesson learned the hard way.
FIXTURE_HISTORICAL = {
    ("comments_bad.rs", "drift.rs:4"): "fixture allowlist witness",
}

# The audit mode's half of the fixtures. `doc/audit.md` and `audit.rs` carry
# FOUR citations: two that must be flagged, one per signal, and two that must
# NOT be. The pair that must stay silent is the point — a heuristic is only as
# good as what it declines to report, and both of these are shapes an earlier
# build of this mode reported by the dozen on the real tree.
#
# `doc/audit.md` is NOT in BROKEN_FIXTURES: every one of its four citations
# resolves to a non-blank line, so rules 1 to 5 must stay silent on it and the
# clean-tree run must still exit 0. The audit is the only thing that reads it.
FIXTURE_AUDIT_EXPECTED = [
    "doc/audit.md:3: `audit.rs:10`",
    "doc/audit.md:7: `audit.rs:11`",
]

# The two signals, asserted by the text that DISCRIMINATES them rather than by
# list position. Both findings would still be two findings if the comment
# signal fired on the symbol case and vice versa, and that swap is exactly what
# a rewrite gets wrong.
FIXTURE_AUDIT_COMMENT = "the target is a `//` comment while the prose names a statement or call"
FIXTURE_AUDIT_SYMBOL = "the prose names `settle_fee`"

# The two that must stay silent, named by their citation text.
#
#   * `audit.rs:10-13` is a RANGE starting on the same comment line the flagged
#     single-line citation names. Delete the `start == end` guard and this
#     becomes a finding — and with it the 13 correct range citations measured
#     on the real tree at `6172929e`.
#   * `audit.rs:30` sits 14 lines below its own `fn`, so only `enclosing_names`
#     answers for it. Delete that and this becomes a finding — and with it the
#     16 correct ones measured on the same tree.
FIXTURE_AUDIT_QUIET = ("audit.rs:10-13", "audit.rs:30")

# What the audit looked at, and the largest thing it declined to look at. The
# second is asserted for rule 2's own reason: a mode that reports what it
# checked and stays quiet about what it skipped reads as complete coverage, and
# 26 of the fixture tree's citations are prose this heuristic cannot test at
# all. That number is the honest half of a clean audit.
FIXTURE_AUDIT_EXAMINED = 11
FIXTURE_AUDIT_UNTESTABLE = 26
FIXTURE_AUDIT_UNTESTABLE_REASON = "prose naming nothing this heuristic can test"


# Every fixture document that is MEANT to fail, removed for the clean-tree run.
BROKEN_FIXTURES = (
    "doc/bad.md",
    "tests_bad.rs",
    "CHANGELOG.md",
    "doc/pinned_bad.md",
    "comments_bad.rs",
    "mod/comments_sibling_bad.rs",
)


def _run_cli(args):
    """Run this script as CI runs it, and return (exit code, output)."""
    proc = subprocess.run(
        [sys.executable, os.path.abspath(__file__)] + args,
        capture_output=True,
        text=True,
    )
    return proc.returncode, proc.stdout + proc.stderr


def _base_variants(tree):
    """Every `<name>.base` in `tree`, paired with the file it is a revision of."""
    pairs = []
    for dirpath, dirnames, filenames in os.walk(tree):
        dirnames[:] = [d for d in dirnames if d not in SKIP_DIRS]
        for name in filenames:
            if name.endswith(DRIFT_BASE_SUFFIX):
                path = os.path.join(dirpath, name)
                pairs.append((path, path[: -len(DRIFT_BASE_SUFFIX)]))
    return sorted(pairs)


def _commit_all(repo, message):
    """Commit a throwaway repository wholesale and return the commit sha.

    The operator's own git configuration is cut out: a global `commit.gpgsign`
    would make this hang on a hardware key, and a global `core.hooksPath` or
    commit template would make the self-test depend on the machine it runs on.
    """
    env = dict(os.environ)
    env["GIT_CONFIG_GLOBAL"] = os.devnull
    env["GIT_CONFIG_SYSTEM"] = os.devnull
    env["GIT_CONFIG_NOSYSTEM"] = "1"
    settings = [
        "-c", "user.name=citation self-test",
        "-c", "user.email=self-test@invalid",
        "-c", "commit.gpgsign=false",
        "-c", "init.defaultBranch=main",
    ]
    for args in (["init", "-q"], ["add", "-A"], ["commit", "-q", "-m", message]):
        subprocess.run(
            ["git", "-C", repo] + settings + args,
            check=True, capture_output=True, text=True, encoding="utf-8", env=env,
        )
    done = subprocess.run(
        ["git", "-C", repo, "rev-parse", "HEAD"],
        check=True, capture_output=True, text=True, encoding="utf-8", env=env,
    )
    return done.stdout.strip()


def _drift_repository(tmp, fixtures):
    """A one-commit git repository holding the fixtures at BOTH revisions.

    The working tree ends up at the head revision and the single commit holds
    the base one, which is exactly the shape a pull request presents: edited
    files on disk, the revision they departed from in the object store. The
    documents that rule 1 and rule 3 are MEANT to fail are removed, so any
    non-zero exit from this tree belongs to the drift rule alone.
    """
    repo = os.path.join(tmp, "drift")
    shutil.copytree(fixtures, repo)
    for broken in BROKEN_FIXTURES:
        os.remove(os.path.join(repo, *broken.split("/")))
    for base, live in _base_variants(repo):
        shutil.copyfile(base, live)
    sha = _commit_all(repo, "base revision of the citation drift fixture")
    for base, live in _base_variants(repo):
        shutil.copyfile(os.path.join(fixtures, os.path.relpath(live, repo)), live)
    return repo, sha


def self_test():
    """Prove the resolver fails on breakage instead of exiting 0 vacuously.

    A guard that always passes is the classic shape of this defect, so the
    fixtures below carry one instance of every failure class and the run
    asserts the exact expected verdicts.
    """
    fixtures = os.path.join(os.path.dirname(os.path.abspath(__file__)), "testdata", "citations")
    total, failures = check(fixtures)
    ok = True

    # Everything that is not the deliberately broken document must be clean —
    # named by exclusion rather than by listing `doc/good.md`, so a fixture
    # added later is covered the day it lands instead of the day someone
    # remembers to extend this line.
    good = [f for f in failures if not f.startswith("doc/bad.md")]
    if good:
        ok = False
        print("FAIL self-test: a clean fixture produced failures:")
        for line in good:
            print("  " + line)

    bad = [f for f in failures if f.startswith("doc/bad.md")]
    if len(bad) != len(FIXTURE_EXPECTED):
        ok = False
        print(
            "FAIL self-test: expected %d failures from the broken fixture, got %d:"
            % (len(FIXTURE_EXPECTED), len(bad))
        )
        for line in bad:
            print("  " + line)
    else:
        for expected, actual in zip(FIXTURE_EXPECTED, bad):
            if not actual.startswith(expected):
                ok = False
                print("FAIL self-test: expected a failure starting %r, got %r" % (expected, actual))

    # The bare continuation, asserted by name. Every other assertion in this
    # block reads a COUNT, and a count that shrinks says only that something is
    # gone; this one says which citation.
    if not any(line.startswith(FIXTURE_BARE_EXPECTED) for line in bad):
        ok = False
        print(
            "FAIL self-test: the bare continuation `:99` on doc/bad.md:9 was not reported. "
            "It inherits `sample.rs` from the citation earlier on its own line and names "
            "line 99 of a 10-line file, so expected a failure starting %r. A citation that "
            "is extracted by nothing is checked by nothing and is never reported as "
            "unresolvable either — sozu-proxy/sozu#1459." % FIXTURE_BARE_EXPECTED
        )

    # The scope and the group form, each asserted by the fixture whose verdict
    # CHANGES when it is weakened. A count cannot stand in for any of these:
    # widening the scope keeps every citation extracted and only moves what it
    # binds to.
    for expected, what in (
        (FIXTURE_BARE_NEAREST_EXPECTED,
         "a continuation must inherit from the NEAREST citation on its line, not the first: "
         "doc/bad.md:22 carries `h2.rs:1` and `sample.rs:3` before its bare `:99`, so binding "
         "to the first would report `h2.rs` (3 lines) instead"),
        (FIXTURE_BARE_SAME_LINE_EXPECTED,
         "a continuation must inherit on its OWN LINE only: doc/bad.md:30's bare `:7` sits one "
         "line below a `sample.rs:3` in the same paragraph, and a paragraph-wide carry binds it "
         "to a real non-blank line, so this failure disappears and the scope silently widens"),
        (FIXTURE_BARE_GROUP_EXPECTED,
         "a bare GROUP continuation must be extracted: doc/bad.md:24's `:3/3` is the "
         "half-applied renumbering of a `/` group with its path dropped, and a `BARE_CITATION` "
         "without CITATION's span group matches it not at all"),
    ):
        if not any(line.startswith(expected) for line in bad):
            ok = False
            print(
                "FAIL self-test: expected a failure starting %r — %s." % (expected, what)
            )

    if total != FIXTURE_TOTAL:
        ok = False
        print(
            "FAIL self-test: extracted %d citations from the fixtures, expected exactly %d — "
            "the scanned surface or the extraction pattern has shrunk" % (total, FIXTURE_TOTAL)
        )

    # ── Rule 2 ────────────────────────────────────────────────────────────
    # A pair of revisions in a throwaway repository. Everything rule 1 and
    # rule 3 are meant to fail has been removed from it, so the SAME tree that
    # exits 0 without `--base` must exit 1 with it: that contrast is the whole
    # rule, and asserting both directions is what proves the drift is not
    # something the older rules were catching all along.
    with tempfile.TemporaryDirectory() as tmp:
        repo, base_sha = _drift_repository(tmp, fixtures)

        resolved, why = resolve_base(repo, base_sha)
        if resolved != base_sha:
            ok = False
            print("FAIL self-test: the base revision did not resolve: %s" % why)

        compared, exempt, reused, drifted = check_drift(repo, base_sha)
        drifted = sorted(drifted)

        # The wide-line drift, separated before the exact comparison because
        # what it asserts is a property of the REPORT rather than its wording.
        wide = [line for line in drifted if line.startswith(FIXTURE_DRIFT_WIDE_PREFIX)]
        drifted = [line for line in drifted if not line.startswith(FIXTURE_DRIFT_WIDE_PREFIX)]
        if len(wide) != 1:
            ok = False
            print(
                "FAIL self-test: expected exactly 1 drift on `doc/wide.md`'s catalogue row, "
                "got %d — the fixture whose difference falls past QUOTE_WIDTH is not firing, "
                "so nothing here proves the report distinguishes the two lines it prints"
                % len(wide)
            )
            for line in wide:
                print("  " + line)
        else:
            pair = REPORTED_PAIR.search(wide[0])
            if pair is None:
                ok = False
                print(
                    "FAIL self-test: could not read the quoted pair out of the drift report "
                    "%r — its shape changed and this assertion now checks nothing" % wide[0]
                )
            else:
                was_quoted, now_quoted = pair.group(1), pair.group(2)
                if was_quoted == now_quoted:
                    ok = False
                    print(
                        "FAIL self-test: the drift on a line whose difference falls past "
                        "QUOTE_WIDTH (%d) reported the SAME text twice, so the report names "
                        "nothing that moved:" % QUOTE_WIDTH
                    )
                    print("    was: " + was_quoted)
                    print("    now: " + now_quoted)
                for half, text in (("was", was_quoted), ("now", now_quoted)):
                    if len(text) > QUOTE_WIDTH:
                        ok = False
                        print(
                            "FAIL self-test: the `%s` half of that report is %d characters, "
                            "above QUOTE_WIDTH (%d) — the clip was raised or removed rather "
                            "than moved onto the difference, and an unbounded quote wraps a "
                            "CI log" % (half, len(text), QUOTE_WIDTH)
                        )

        if len(drifted) != len(FIXTURE_DRIFT_EXPECTED):
            ok = False
            print(
                "FAIL self-test: expected %d drifted citations from the fixtures, got %d:"
                % (len(FIXTURE_DRIFT_EXPECTED), len(drifted))
            )
            for line in drifted:
                print("  " + line)
        else:
            for expected, actual in zip(FIXTURE_DRIFT_EXPECTED, drifted):
                if actual != expected:
                    ok = False
                    print("FAIL self-test: expected the drift %r, got %r" % (expected, actual))

        if compared != FIXTURE_DRIFT_COMPARED:
            ok = False
            print(
                "FAIL self-test: compared %d cited lines against the base revision, expected "
                "exactly %d — the scanned surface or the comparison has shrunk"
                % (compared, FIXTURE_DRIFT_COMPARED)
            )

        # The exemption, asserted rather than described. Every other count here
        # measures what the rule looked at; only this one measures what it
        # declined to look at, which is the single number that separates "38
        # compared" read as complete coverage from "38 compared, 12 exempt"
        # read as something to disposition. A build that stopped emitting it —
        # or an exemption widened until it swallowed the comparison — moves
        # this and nothing else.
        if exempt != FIXTURE_DRIFT_EXEMPT:
            ok = False
            print(
                "FAIL self-test: %d cited lines were exempt from the comparison as re-anchored, "
                "expected exactly %d. A compared total reports what the rule looked at and never "
                "what it declined to look at, so this rule can report a healthy count while "
                "covering nothing — sozu-proxy/sozu#1447." % (exempt, FIXTURE_DRIFT_EXEMPT)
            )

        # The second declined class, asserted by its exact REPORTED TEXT and
        # not by a count. This is the only verdict rule 2 reaches by comparing
        # text across revisions instead of by asking whether a number was
        # present, so the evidence it prints is half the guarantee: an exemption
        # a reviewer cannot read is a heuristic with no audit trail, which is
        # the thing sozu-proxy/sozu#1447 refused. Asserting the string also
        # keeps the report's SHAPE pinned — a build that stopped naming the
        # matched text, or the number the claim moved to, emits the same count
        # and fails here.
        reused = sorted(reused)
        if reused != FIXTURE_DRIFT_REUSED_EXPECTED:
            ok = False
            print(
                "FAIL self-test: expected %d cited line(s) declined as a reused number, got %d:"
                % (len(FIXTURE_DRIFT_REUSED_EXPECTED), len(reused))
            )
            for line in reused:
                print("  " + line)
            for expected in FIXTURE_DRIFT_REUSED_EXPECTED:
                if expected not in reused:
                    print("  MISSING: " + expected)

        # The same tree, without a base: every drift above is invisible to the
        # blank-line rule because every drifted line is non-blank at both
        # revisions. This is the red half of the rule, asserted rather than
        # described.
        code, out = _run_cli(["--root", repo])
        if code != 0:
            ok = False
            print(
                "FAIL self-test: the drift fixture exited %d WITHOUT `--base`, expected 0 — "
                "its drifts must be invisible to the other two rules, or this fixture is "
                "not testing the drift rule" % code
            )
            print("".join("    " + line + "\n" for line in out.splitlines()))

        code, out = _run_cli(["--root", repo, "--base", base_sha])
        if code != 1 or "citations drifted" not in out:
            ok = False
            print(
                "FAIL self-test: the drift fixture exited %d WITH `--base`, expected 1 and a "
                "drift report — the rule classifies without acting on it" % code
            )
            print("".join("    " + line + "\n" for line in out.splitlines()))

        # Fail closed. An unreachable base is the normal state of a shallow
        # `actions/checkout`, and a guard that answered it with a clean run
        # would be green forever while comparing nothing.
        code, out = _run_cli(["--root", repo, "--base", "0" * 40])
        if code != 1 or "names no commit in this checkout" not in out:
            ok = False
            print(
                "FAIL self-test: an unreachable `--base` exited %d, expected 1 and a refusal — "
                "the drift rule must never treat a missing base as nothing to compare" % code
            )
            print("".join("    " + line + "\n" for line in out.splitlines()))

    # ── Rule 3 ────────────────────────────────────────────────────────────
    # Default tables first: nothing in them names anything in the fixture
    # tree, so all three broken citations must report as plainly dead.
    examined, checked, dead = check_test_citations(fixtures)
    dead = sorted(dead)
    if len(dead) != len(FIXTURE_TEST_EXPECTED):
        ok = False
        print(
            "FAIL self-test: expected %d dead test-name citations from the fixtures, got %d:"
            % (len(FIXTURE_TEST_EXPECTED), len(dead))
        )
        for line in dead:
            print("  " + line)
    else:
        for expected, actual in zip(FIXTURE_TEST_EXPECTED, dead):
            if not actual.startswith(expected):
                ok = False
                print("FAIL self-test: expected a failure starting %r, got %r" % (expected, actual))

    if (examined, checked) != (FIXTURE_TEST_EXAMINED, FIXTURE_TEST_CHECKED):
        ok = False
        print(
            "FAIL self-test: examined %d / checked %d candidate test names, expected %d / %d — "
            "the scanned surface or one of the two filters has moved"
            % (examined, checked, FIXTURE_TEST_EXAMINED, FIXTURE_TEST_CHECKED)
        )

    # Now the two dispositions. With the fixture tables, the allowlisted name
    # and the rename with a LIVE target must both disappear, and the rename
    # whose target is itself absent must still be reported — an allowlist that
    # accepted every entry unconditionally would pass the run above and turn
    # this rule off one line at a time.
    _, _, disposed = check_test_citations(
        fixtures, renamed=FIXTURE_RENAMED, not_a_test=FIXTURE_NOT_A_TEST
    )
    if len(disposed) != 1 or "which names no `fn` in the tree either" not in disposed[0]:
        ok = False
        print(
            "FAIL self-test: with the fixture tables, expected exactly one failure — the rename "
            "whose target is also gone — got %d:" % len(disposed)
        )
        for line in disposed:
            print("  " + line)

    # ── Rule 4 ────────────────────────────────────────────────────────────
    # The broken document's citation resolves, so rule 1 passes it; asserting
    # that above (`good` is empty for every document but `doc/bad.md`) is what
    # makes this a rule and not a restatement of the resolver.
    pinned, unpinned, mismatched = check_pinned_snippets(fixtures)
    mismatched = sorted(mismatched)
    if len(mismatched) != len(FIXTURE_PINNED_EXPECTED):
        ok = False
        print(
            "FAIL self-test: expected %d mismatched pinned blocks from the fixtures, got %d:"
            % (len(FIXTURE_PINNED_EXPECTED), len(mismatched))
        )
        for line in mismatched:
            print("  " + line)
    else:
        for expected, actual in zip(FIXTURE_PINNED_EXPECTED, mismatched):
            if actual != expected:
                ok = False
                print("FAIL self-test: expected the mismatch %r, got %r" % (expected, actual))

    if (pinned, unpinned) != (FIXTURE_PINNED, FIXTURE_UNPINNED):
        ok = False
        print(
            "FAIL self-test: compared %d pinned blocks and left %d unpinned Rust blocks alone, "
            "expected exactly %d / %d — the annotation pattern or the scanned surface has moved"
            % (pinned, unpinned, FIXTURE_PINNED, FIXTURE_UNPINNED)
        )

    # ── Rule 5 ──────────────────────────────────────────────────────
    # Default table first: its one entry names a file in the REAL tree and
    # nothing in the fixtures, so the historical witness must report as plainly
    # forbidden here alongside the rest. Every citation in `comments_good.rs`
    # must stay silent, and it is not named by exclusion the way rule 1's clean
    # set is — `external` carries that, as a counted total rather than an
    # absence.
    seen, external, comment_exempt, cited_lines = check_rust_comment_citations(fixtures)
    cited_lines = sorted(cited_lines)
    if len(cited_lines) != len(FIXTURE_COMMENT_EXPECTED):
        ok = False
        print(
            "FAIL self-test: expected %d line-number citations from the Rust fixtures, got %d:"
            % (len(FIXTURE_COMMENT_EXPECTED), len(cited_lines))
        )
        for line in cited_lines:
            print("  " + line)
    else:
        for expected, actual in zip(FIXTURE_COMMENT_EXPECTED, cited_lines):
            if not actual.startswith(expected):
                ok = False
                print("FAIL self-test: expected a failure starting %r, got %r" % (expected, actual))

    # The bare SIBLING citation, asserted by name. The list above cannot stand
    # in for it: bind `h2.rs:2` to the repository root instead of the citing
    # file's directory and it is still reported, still one entry, still a real
    # non-blank line — only the resolved target in the text changes, from
    # `mod/h2.rs` to `h2.rs`. That is the same failure shape as a citation that
    # resolves onto unrelated code: plausible, confident, and wrong.
    if not any(line.startswith(FIXTURE_COMMENT_SIBLING_EXPECTED) for line in cited_lines):
        ok = False
        print(
            "FAIL self-test: the bare sibling citation `h2.rs:2` on "
            "mod/comments_sibling_bad.rs:5 was not reported against mod/h2.rs. A cited path "
            "binds to the citing file's own directory first, which is what brings a bare "
            "`h1.rs:NNN` written from `mux/h2.rs` inside this rule; a prefix-keyed rule walks "
            "past that whole shape. Expected a failure starting %r."
            % FIXTURE_COMMENT_SIBLING_EXPECTED
        )

    if (seen, external, comment_exempt) != (FIXTURE_COMMENT_EXAMINED, FIXTURE_COMMENT_EXTERNAL, 0):
        ok = False
        print(
            "FAIL self-test: examined %d / %d external / %d exempt Rust-comment citations, "
            "expected %d / %d / 0 — the scanned surface, the comment-line test or the "
            "external skip has moved"
            % (seen, external, comment_exempt, FIXTURE_COMMENT_EXAMINED, FIXTURE_COMMENT_EXTERNAL)
        )

    # Now the disposition. With the fixture table the historical witness must
    # disappear from the failures AND appear in `exempt`; a table that silenced
    # an entry without counting it would pass the first half of that and put
    # this rule's coverage somewhere no report can read it.
    _, _, table_exempt, disposed = check_rust_comment_citations(
        fixtures, historical=FIXTURE_HISTORICAL
    )
    if len(disposed) != len(FIXTURE_COMMENT_EXPECTED) - 1 or table_exempt != 1:
        ok = False
        print(
            "FAIL self-test: with the fixture table, expected exactly %d failures and 1 exempt, "
            "got %d and %d:"
            % (len(FIXTURE_COMMENT_EXPECTED) - 1, len(disposed), table_exempt)
        )
        for line in sorted(disposed):
            print("  " + line)
    elif any("drift.rs:4" in line for line in disposed):
        ok = False
        print(
            "FAIL self-test: the fixture table declares `drift.rs:4` historical and it was "
            "still reported — HISTORICAL_CITATIONS is keyed on (citing file, citation text), "
            "and a key that does not match exempts nothing while looking like it does."
        )

    # ── The audit mode ────────────────────────────────────────────────────
    # Not a rule: it reports and never gates, so the assertions are about WHAT
    # it says, not about an exit code. Two findings, one per signal, and two
    # citations it must leave alone.
    audit_examined, audit_declined, audit_findings = audit(fixtures)
    audit_findings = sorted(audit_findings)
    if len(audit_findings) != len(FIXTURE_AUDIT_EXPECTED):
        ok = False
        print(
            "FAIL self-test: expected %d audit findings from the fixtures, got %d:"
            % (len(FIXTURE_AUDIT_EXPECTED), len(audit_findings))
        )
        for line in audit_findings:
            print("  " + line)
    else:
        for expected, actual in zip(FIXTURE_AUDIT_EXPECTED, audit_findings):
            if not actual.startswith(expected):
                ok = False
                print("FAIL self-test: expected an audit finding starting %r, got %r"
                      % (expected, actual.splitlines()[0]))

    # Each signal fires on ITS OWN case. Asserted separately because the counts
    # above cannot tell one signal from the other, and a build where the
    # comment signal answered for the symbol case would pass every count here.
    for expected, reason, what in (
        (FIXTURE_AUDIT_EXPECTED[0], FIXTURE_AUDIT_COMMENT,
         "`audit.rs:10` is a `//` comment cited by prose naming a call — the signal "
         "sozu-proxy/sozu#1466 named, and the one that catches three of its four"),
        (FIXTURE_AUDIT_EXPECTED[1], FIXTURE_AUDIT_SYMBOL,
         "`audit.rs:11` is ordinary code whose only tell is that `settle_fee` is not "
         "there — the signal that catches #1466's FOURTH citation, which lands on "
         "`let total_before = *total;` and looks perfectly healthy"),
    ):
        if not any(line.startswith(expected) and reason in line for line in audit_findings):
            ok = False
            print(
                "FAIL self-test: expected the audit finding %s to carry %r — %s."
                % (expected, reason, what)
            )

    # The two it must leave alone. This is the half that keeps the mode usable:
    # a heuristic tuned until it reports everything is the same as one tuned
    # until it reports nothing.
    for quiet in FIXTURE_AUDIT_QUIET:
        if any(quiet in line for line in audit_findings):
            ok = False
            print(
                "FAIL self-test: the audit reported `%s`, which must stay silent. See "
                "FIXTURE_AUDIT_QUIET for what each of the two pins and how many correct "
                "citations on the real tree go with it." % quiet
            )

    declined_counts = dict(audit_declined)
    if audit_examined != FIXTURE_AUDIT_EXAMINED:
        ok = False
        print(
            "FAIL self-test: the audit examined %d fixture citations, expected exactly %d — "
            "the scanned surface or one of the two prose gates has moved"
            % (audit_examined, FIXTURE_AUDIT_EXAMINED)
        )
    if declined_counts.get(FIXTURE_AUDIT_UNTESTABLE_REASON) != FIXTURE_AUDIT_UNTESTABLE:
        ok = False
        print(
            "FAIL self-test: the audit declined %r for %r citations, expected exactly %d. "
            "An audit that counts what it checked and not what it skipped reads as complete "
            "coverage — sozu-proxy/sozu#1457, sozu-proxy/sozu#1447."
            % (FIXTURE_AUDIT_UNTESTABLE_REASON,
               declined_counts.get(FIXTURE_AUDIT_UNTESTABLE_REASON), FIXTURE_AUDIT_UNTESTABLE)
        )

    # And the command line, because `--audit` must NOT act on what it finds.
    # This is the inverse of every other CLI assertion here: the broken tree
    # exits 1 for the five rules and the audit over the same tree exits 0.
    code, out = _run_cli(["--audit", "--root", fixtures])
    if (code != 0 or "ADVISORY" not in out
            or FIXTURE_AUDIT_COMMENT not in out or FIXTURE_AUDIT_SYMBOL not in out):
        ok = False
        print(
            "FAIL self-test: `--audit` over the fixture tree exited %d, expected 0 with an "
            "advisory banner and both findings — a heuristic that fails a build is one "
            "people learn to silence" % code
        )
        print("".join("    " + line + "\n" for line in out.splitlines()))

    # `check()` classifying correctly is NOT the same as the command acting on
    # it. A build of this script that reports every failure and still exits 0
    # is green in CI and guards nothing, and nothing above this point executes
    # the line that decides the exit code. So run the real command line, in a
    # subprocess, in both directions.
    code, out = _run_cli(["--root", fixtures])
    if code != 1:
        ok = False
        print(
            "FAIL self-test: a run over the BROKEN fixture tree exited %d, expected 1 — "
            "the checker reports failures without acting on them" % code
        )
        print("".join("    " + line + "\n" for line in out.splitlines()))

    with tempfile.TemporaryDirectory() as tmp:
        # The whole fixture tree minus the documents that are meant to fail:
        # copying it wholesale keeps this half honest as fixtures are added.
        clean = os.path.join(tmp, "clean")
        shutil.copytree(fixtures, clean)
        for broken in BROKEN_FIXTURES:
            os.remove(os.path.join(clean, *broken.split("/")))
        code, out = _run_cli(["--root", clean])
    if code != 0:
        ok = False
        print("FAIL self-test: a run over the CLEAN fixture tree exited %d, expected 0" % code)
        print("".join("    " + line + "\n" for line in out.splitlines()))

    if ok:
        print(
            "OK self-test: %d fixture line citations, %d of them compared against a base "
            "revision (%d more exempt as re-anchored, %d declined as a reused number), "
            "%d examined test names (%d checked), "
            "%d pinned blocks compared (%d unpinned Rust blocks left alone), and %d citations "
            "seen in Rust comments (%d naming no file in the fixture tree); %d + %d + %d "
            "+ %d + %d expected failures reported, the fixture exemption table moving one "
            "verdict, exit 1 on the broken tree and 0 on the clean one, and an unreachable "
            "base refused instead of skipped. The audit examined %d of them and reported "
            "%d, one per signal, leaving a comment-started RANGE and a line answered only "
            "by its enclosing item alone, and exiting 0 on a tree it found things in."
            % (
                total, compared, exempt, len(reused), examined, checked, pinned, unpinned,
                seen, external,
                len(bad), len(drifted), len(dead), len(mismatched), len(cited_lines),
                audit_examined, len(audit_findings),
            )
        )
    return 0 if ok else 1


def _spans(count):
    """`3 re-anchored spans were` / `1 re-anchored span was`, for a report line.

    The exempt count exists to be READ by a reviewer deciding whether a green
    run covered anything, so it is worth the four lines it takes to not say
    "1 spans were".
    """
    return "%d re-anchored span%s" % (count, " was" if count == 1 else "s were")


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--root", default=".", help="repository root (default: .)")
    parser.add_argument(
        "--base",
        help="revision this changeset departed from; enables the drifted-citation rule. "
        "Its merge base with HEAD is what is compared against, and a revision that is not "
        "in this checkout is an error, never a skip.",
    )
    parser.add_argument("--show", action="store_true", help="print every resolved citation")
    parser.add_argument("--self-test", action="store_true", help="run the fixture self-test and exit")
    parser.add_argument(
        "--audit",
        action="store_true",
        help="read every citation's prose against its cited line and report what does not "
        "plainly match, then exit 0. ADVISORY and standalone: it runs none of the five "
        "rules, never fails a build, and is deliberately not in the CI job.",
    )
    args = parser.parse_args()

    if args.self_test:
        return self_test()

    if args.audit:
        return audit_report(os.path.abspath(args.root), show=args.show)

    root = os.path.abspath(args.root)
    status = 0

    total, failures = check(root, show=args.show)
    if failures:
        status = 1
        print("::error::%d of %d `file.rs:NNN` / `file.md:NNN` citations (bare `:NNN` continuations included) in doc/ and **/LIFECYCLE.md do not resolve:" % (len(failures), total))
        for line in failures:
            print("  " + line)
        print("")
        print("Cite a symbol (`Type::method`) where the prose names an item — a symbol cannot drift.")
        print("Keep a line or a range only where the prose means a specific branch inside an item.")
        print("A bare `:NNN` continues the citation earlier on its own line; with none there, write the path.")
        print("Convention and local usage: doc/README.md#citing-code-from-these-documents")
    else:
        print("OK: all %d `file.rs:NNN` / `file.md:NNN` citations (bare `:NNN` continuations included) in doc/ and **/LIFECYCLE.md resolve to a non-blank line." % total)

    print("")
    if not args.base:
        print("SKIPPED: the drifted-citation rule needs `--base <revision>` and did not run.")
        print("Without it the rule above is a floor, not a proof: a citation that drifted onto a")
        print("different non-blank line resolves, hits code, and passes. On sozu-proxy/sozu#1379's")
        print("changeset 24 citations moved and this run would have reported 2 (sozu#1389).")
    else:
        base, why = resolve_base(root, args.base)
        if base is None:
            print("::error::the drifted-citation rule cannot run: %s" % why)
            print("")
            print("This is an error and not a skip on purpose. A guard that answered a missing base")
            print("with a clean run would be green forever while comparing nothing, which is the")
            print("shape of defect this file exists to close.")
            return 1
        compared, exempt, reused, drifted = check_drift(root, base, show=args.show)
        if drifted:
            status = 1
            print(
                "::error::%d of %d compared citations drifted since %s (%d re-anchored span%s exempt):"
                % (len(drifted), compared, base[:12], exempt, "" if exempt == 1 else "s")
            )
            for line in drifted:
                print("  " + line)
            print("")
            print("The cited line moved and the citation did not follow it. Renumber it, or better,")
            print("replace it with the symbol the prose already names — a symbol cannot drift.")
            print("A span this changeset re-anchored is exempt and counted above, never silent; a")
            print("sibling it left behind in the same group is still compared.")
            print("Read each against the claim before renumbering. Citing a symbol wherever the prose")
            print("names an item leaves this whole class behind for good.")
        else:
            print(
                "OK: none of the %d cited lines compared against %s changed their text."
                % (compared, base[:12])
            )
            print("Still a floor for a line this changeset did not touch: only drift SINCE the base")
            print("is visible, so cite a symbol wherever the prose names an item.")
            print(
                "%s exempt from the comparison and never checked at all; `--show` lists them, "
                "and a compared total cannot report them." % _spans(exempt)
            )

        if reused:
            print("")
            print(
                "%d cited line%s NOT compared because this changeset reused the number for "
                "different code (sozu-proxy/sozu#1447):"
                % (len(reused), "" if len(reused) == 1 else "s")
            )
            for line in reused:
                print("  " + line)
            print("")
            print("Each is declined on EVIDENCE, not on a guess: the text that number held at the base")
            print("is cited again at a number this changeset introduced, at least as many times as the")
            print("base document named it, so the claim moved and the number was spent on something")
            print("else. Without this the correct citation is reported as drift and no edit to the")
            print("document removes the report. Two residuals stay, and both are printed rather than")
            print("silent. A base citation this changeset DELETED instead of re-anchoring leaves no")
            print("such number, so a correct citation landing on the deleted one's line is still")
            print("reported above. And a cited line carrying little text — a lone `}`, a bare `where`")
            print("— can match by coincidence, and a real drift is declined; the matched text is")
            print("quoted on each line so a reviewer can see what it was and disposition it.")
            print("A symbol has no number to reuse, and none of this applies to one.")

    print("")
    examined, checked, dead = check_test_citations(root, show=args.show)
    if dead:
        status = 1
        print(
            "::error::%d of %d cited test names (from %d candidate identifiers) name no `fn` in the tree:"
            % (len(dead), checked, examined)
        )
        for line in dead:
            print("  " + line)
        print("")
        print("Repoint the citation at the test that exists, write the test the prose claims, or drop the claim.")
        print("A name deliberately kept as a former one belongs in RENAMED_TESTS, with the name it carries now;")
        print("an identifier that is not a test name at all belongs in NOT_A_TEST, with the reason.")
    else:
        print(
            "OK: all %d cited test names (from %d candidate identifiers) name a `fn` in the tree."
            % (checked, examined)
        )
        print("This too is a floor: a citation naming a real but unrelated `fn` still passes.")

    print("")
    pinned, unpinned, mismatched = check_pinned_snippets(root, show=args.show)
    if mismatched:
        status = 1
        print(
            "::error::%d of %d pinned code blocks no longer quote the lines they name:"
            % (len(mismatched), pinned)
        )
        for line in mismatched:
            print("  " + line)
        print("")
        print("The quoted code changed and the block did not follow it. Re-quote the cited lines")
        print("verbatim, repoint the annotation at the lines the block really shows, or drop the")
        print("annotation — an abbreviated or illustrative snippet is not meant to carry one.")
    else:
        print(
            "OK: all %d pinned code blocks quote their cited lines verbatim (%d unannotated "
            "Rust blocks were not compared)." % (pinned, unpinned)
        )
        print("This is a floor too, and an opt-in one: a block that carries no `path:NNN-MMM` in its")
        print("fence is never compared, so a quote is only guarded once its author pins it.")

    print("")
    seen, external, exempt, cited_lines = check_rust_comment_citations(root, show=args.show)
    if cited_lines:
        status = 1
        print(
            "::error::%d of %d `file.rs:NNN` citations written in a Rust comment name a line in "
            "this tree:" % (len(cited_lines), seen)
        )
        for line in cited_lines:
            print("  " + line)
        print("")
        print("This rule FORBIDS the form rather than resolving it. A line number in a comment is a")
        print("pointer with no owner: the resolver above reads `doc/**` and every `**/LIFECYCLE.md`")
        print("and never a Rust comment, so until this rule the form was checked by nothing at all")
        print("(sozu-proxy/sozu#1466, sozu-proxy/sozu#1473). Measured at main `19fd5d8c`, 68 of the")
        print("tree's 90 such citations had already drifted and the median one survived ONE commit")
        print("into the file it points at.")
        print("Cite the SYMBOL — `Type::method`, with the path and no line number. Where the prose")
        print("means one branch inside an item, name that branch in words. A citation that records a")
        print("position at a revision which no longer exists belongs in HISTORICAL_CITATIONS, keyed")
        print("on its citing file and its exact text, with the reason — never renumbered onto")
        print("today's tree, which falsifies the record instead of repairing it.")
        print("Convention and worked example: doc/README.md#citing-code-from-a-rust-comment")
    else:
        print(
            "OK: no `file.rs:NNN` citation in a Rust comment names a line in this tree "
            "(%d seen; %d name no file here, %d exempt as historical)." % (seen, external, exempt)
        )
        print("Every `*.rs` in the tree is scanned, not `lib|command|bin|e2e/src` alone, and a cited")
        print("path binds to the citing file's own directory first — so a bare `h1.rs:NNN` written")
        print("from `mux/h2.rs` is inside this rule, although a prefix-keyed grep walks past it.")
        print("What it declines to look at is counted beside what it checked, never silent: a")
        print("citation naming no file here is a pinned external dependency this repository does not")
        print("edit, and a TRAILING comment on a code line is not read at all.")

    return status


if __name__ == "__main__":
    sys.exit(main())
