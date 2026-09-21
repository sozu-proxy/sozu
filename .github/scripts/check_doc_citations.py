#!/usr/bin/env python3
# Resolve every citation in this repository's prose against the tree it ships
# with, and fail when one of them cannot be resolved. Two citation forms are
# checked, by two independent rules:
#
#   1. `file.rs:NNN(-MMM)?` in `doc/**` and `**/LIFECYCLE.md` — a path and a
#      line number. See "WHAT THIS CATCHES" below.
#   2. a backticked TEST NAME, in a Rust comment or a CHANGELOG/doc paragraph,
#      that names no `fn` anywhere in the tree. See "DEAD TEST-NAME CITATIONS"
#      further down.
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
#
# WHAT THIS DOES NOT CATCH — READ THIS BEFORE TRUSTING A GREEN RUN
#   This is a floor, not a proof. It cannot tell whether a citation lands on
#   *the construct the surrounding prose is talking about*. A citation that
#   drifted from line 118 to line 164 still resolves, still hits code, and
#   still passes here while pointing the reader at a different branch. That is
#   the dominant failure mode, not the exotic one. Measured on the module
#   LIFECYCLE.md files at main `ba7fa5f9`: this resolver flagged 27 of the 507
#   anchors those documents carry, while the hand audit that followed cut them
#   to 238 and had to renumber 159 of the survivors.
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
#   Nothing mechanical closes that gap. The remedy is to cite a *symbol*
#   (`ExpectProxyProtocol::readable`) wherever the prose names an item, and to
#   keep a line or a range only where the prose means a specific branch or
#   statement inside an item. A symbol cannot drift; a line always can. Treat a
#   green run as "no citation is obviously dead", never as "the citations are
#   right".
#
# The regex is deliberately `[A-Za-z0-9_/.-]+\.rs:[0-9]+`. The obvious
# character class `[A-Za-z_/.-]+` has no digit in it and silently skips every
# citation naming a file with a digit in its name — `h1.rs:NNN`, `h2.rs:NNN`,
# which in this tree is the majority of them.
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
# Usage:
#   python3 .github/scripts/check_doc_citations.py            # check the tree
#   python3 .github/scripts/check_doc_citations.py --show     # + print every
#                                                             #   resolved citation
#   python3 .github/scripts/check_doc_citations.py --self-test # prove it fails
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
# A continuation may wrap a line in the prose, so the separator swallows a
# newline plus that line's leading whitespace.
PATH = r"[A-Za-z0-9_][A-Za-z0-9_./-]*\.rs"
SPAN = r"[0-9]+(?:-[0-9]+)?"
CITATION = re.compile(
    rf"(?P<path>{PATH}):(?P<spans>{SPAN}(?:[,/][ \t]*(?:\n[ \t]*)?{SPAN})*)"
)
SPAN_SEP = re.compile(r"[,/][ \t]*(?:\n[ \t]*)?")
# `testdata` holds this script's own fixtures, including a deliberately broken
# document and a `sample.rs` that must never answer a real citation. Skipping
# the directory by name keeps it out of the repository walk while leaving the
# self-test — which roots the same walk *inside* it — unaffected.
SKIP_DIRS = {".git", "target", "node_modules", "testdata"}


def rust_files(root):
    """Every `*.rs` path in the tree, repo-root-relative, with a suffix index."""
    by_suffix = {}
    for dirpath, dirnames, filenames in os.walk(root):
        dirnames[:] = [d for d in dirnames if d not in SKIP_DIRS]
        for name in filenames:
            if not name.endswith(".rs"):
                continue
            rel = os.path.relpath(os.path.join(dirpath, name), root).replace(os.sep, "/")
            parts = rel.split("/")
            for i in range(len(parts)):
                by_suffix.setdefault("/".join(parts[i:]), []).append(rel)
    return by_suffix


def doc_files(root):
    """`doc/**` markdown plus every `**/LIFECYCLE.md` — the guarded surface."""
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


def check(root, show=False, out=sys.stdout):
    by_suffix = rust_files(root)
    cache = {}
    failures = []
    total = 0

    for doc in doc_files(root):
        doc_dir = os.path.dirname(doc)
        with open(os.path.join(root, doc), encoding="utf-8") as handle:
            body = handle.read()
        # Line number of the citation itself, for the error message.
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

        for match in CITATION.finditer(body):
            total += 1
            where = "%s:%d" % (doc, doc_line(match.start()))
            cited = match.group("path")
            spans_text = " ".join(match.group("spans").split())
            spans = parse_spans(match.group("spans"))

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


# ── Rule 2: dead test-name citations ──────────────────────────────────────
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
}

# Sentence-shaped identifiers that survive both filters and are not test names.
# Every entry carries the reason it is here; an entry without one is an
# unreviewed silencing of this rule.
NOT_A_TEST = {
    "h2_graceful_shutdown_deadline_seconds": "listener configuration key",
    "h2_max_rst_stream_per_window": "listener configuration key",
    "select_nth_unstable_by_key": "std library method",
    "project_sozu_h2_flood_family_flakes": "agent memory note, not a test",
    "feedback_h2_repro_multi_data_frames": "agent memory note, not a test",
    "feedback_log_context_before_theorising": "agent memory note, not a test",
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
    """The surface rule 2 reads: every `*.rs`, plus CHANGELOG.md and the docs.

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


FIXTURE_EXPECTED = [
    "doc/bad.md:3: `sample.rs:99` — past end of sample.rs (10 lines)",
    "doc/bad.md:5: `sample.rs:4` — sample.rs:4 is blank",
    "doc/bad.md:7: `nowhere.rs:1` — no such file in the tree",
    "doc/bad.md:9: `sample.rs:3/3` — line 3 repeated inside one citation",
    "doc/bad.md:11: `sample.rs:0` — line numbers start at 1",
    "doc/bad.md:13: `sample.rs:3-5` — sample.rs:5 is blank (end of range)",
    "doc/bad.md:15: `h2.rs:99` — past end of h2.rs (3 lines)",
]

# The exact number of citation groups the fixtures contain. Asserting the total
# — not a floor — is what makes a SHRINKING extraction surface fail the
# self-test, and two such shrinks are one edit each:
#   * dropping the digit from PATH loses every `h2.rs:N` citation. The fixtures
#     carry three, one of them an expected failure, so both this total and
#     FIXTURE_EXPECTED go wrong.
#   * dropping `**/LIFECYCLE.md` from doc_files() loses mod/LIFECYCLE.md's two.
# Neither is an accidental shape, and neither announces itself: on the real
# tree they report a clean run over a quietly smaller surface.
FIXTURE_TOTAL = 14

# Rule 2's half of the fixtures. `tests_bad.rs` and the fixture `CHANGELOG.md`
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

# Every fixture document that is MEANT to fail, removed for the clean-tree run.
BROKEN_FIXTURES = ("doc/bad.md", "tests_bad.rs", "CHANGELOG.md")


def _run_cli(args):
    """Run this script as CI runs it, and return (exit code, output)."""
    proc = subprocess.run(
        [sys.executable, os.path.abspath(__file__)] + args,
        capture_output=True,
        text=True,
    )
    return proc.returncode, proc.stdout + proc.stderr


def self_test():
    """Prove the resolver fails on breakage instead of exiting 0 vacuously.

    A guard that always passes is the classic shape of this defect, so the
    fixtures below carry one instance of every failure class and the run
    asserts the exact expected verdicts.
    """
    fixtures = os.path.join(os.path.dirname(os.path.abspath(__file__)), "testdata", "citations")
    total, failures = check(fixtures)
    ok = True

    good = [f for f in failures if f.startswith("doc/good.md")]
    if good:
        ok = False
        print("FAIL self-test: the clean fixture produced failures:")
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

    if total != FIXTURE_TOTAL:
        ok = False
        print(
            "FAIL self-test: extracted %d citations from the fixtures, expected exactly %d — "
            "the scanned surface or the extraction pattern has shrunk" % (total, FIXTURE_TOTAL)
        )

    # ── Rule 2 ────────────────────────────────────────────────────────────
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
            "OK self-test: %d fixture line citations and %d examined test names (%d checked), "
            "%d + %d expected failures reported, exit 1 on the broken tree and 0 on the clean one."
            % (total, examined, checked, len(bad), len(dead))
        )
    return 0 if ok else 1


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--root", default=".", help="repository root (default: .)")
    parser.add_argument("--show", action="store_true", help="print every resolved citation")
    parser.add_argument("--self-test", action="store_true", help="run the fixture self-test and exit")
    args = parser.parse_args()

    if args.self_test:
        return self_test()

    root = os.path.abspath(args.root)
    status = 0

    total, failures = check(root, show=args.show)
    if failures:
        status = 1
        print("::error::%d of %d `file.rs:NNN` citations in doc/ and **/LIFECYCLE.md do not resolve:" % (len(failures), total))
        for line in failures:
            print("  " + line)
        print("")
        print("Cite a symbol (`Type::method`) where the prose names an item — a symbol cannot drift.")
        print("Keep a line or a range only where the prose means a specific branch inside an item.")
        print("Convention and local usage: doc/README.md#citing-code-from-these-documents")
    else:
        print("OK: all %d `file.rs:NNN` citations in doc/ and **/LIFECYCLE.md resolve to a non-blank line." % total)
        print("This is a floor, not a proof: a citation that drifted onto a different non-blank line still passes.")

    examined, checked, dead = check_test_citations(root, show=args.show)
    if dead:
        status = 1
        print("")
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

    return status


if __name__ == "__main__":
    sys.exit(main())
