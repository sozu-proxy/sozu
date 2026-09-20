#!/usr/bin/env python3
# Resolve every `file.rs:NNN(-MMM)?` citation in `doc/**` and `**/LIFECYCLE.md`
# against the tree it ships with, and fail when one of them cannot be resolved.
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
# Usage:
#   python3 .github/scripts/check_doc_citations.py            # check the tree
#   python3 .github/scripts/check_doc_citations.py --show     # + print targets
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
        # The whole fixture tree minus the one document that is meant to fail:
        # copying it wholesale keeps this half honest as fixtures are added.
        clean = os.path.join(tmp, "clean")
        shutil.copytree(fixtures, clean)
        os.remove(os.path.join(clean, "doc", "bad.md"))
        code, out = _run_cli(["--root", clean])
    if code != 0:
        ok = False
        print("FAIL self-test: a run over the CLEAN fixture tree exited %d, expected 0" % code)
        print("".join("    " + line + "\n" for line in out.splitlines()))

    if ok:
        print(
            "OK self-test: %d fixture citations, %d expected failures reported, "
            "exit 1 on the broken tree and 0 on the clean one." % (total, len(bad))
        )
    return 0 if ok else 1


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--root", default=".", help="repository root (default: .)")
    parser.add_argument("--show", action="store_true", help="print every resolved target line")
    parser.add_argument("--self-test", action="store_true", help="run the fixture self-test and exit")
    args = parser.parse_args()

    if args.self_test:
        return self_test()

    total, failures = check(os.path.abspath(args.root), show=args.show)
    if failures:
        print("::error::%d of %d `file.rs:NNN` citations in doc/ and **/LIFECYCLE.md do not resolve:" % (len(failures), total))
        for line in failures:
            print("  " + line)
        print("")
        print("Cite a symbol (`Type::method`) where the prose names an item — a symbol cannot drift.")
        print("Keep a line or a range only where the prose means a specific branch inside an item.")
        print("Convention and local usage: doc/README.md#citing-code-from-these-documents")
        return 1

    print("OK: all %d `file.rs:NNN` citations in doc/ and **/LIFECYCLE.md resolve to a non-blank line." % total)
    print("This is a floor, not a proof: a citation that drifted onto a different non-blank line still passes.")
    return 0


if __name__ == "__main__":
    sys.exit(main())
