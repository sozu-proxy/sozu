# Identity fixture — a half-applied renumbering, and a line number reused

Every citation here resolves to a non-blank line at both revisions, so the
blank-line rule passes this document in both directions and only a comparison
against the base revision separates the two cases below.

Half-applied: the two bindings this document tracks sit at `keyed.rs:12/17`.
This changeset moved the first span onto its true new line and left the second
one behind, so the second must still be compared — a moved sibling is not an
exemption for the rest of its group.

The tail helper is declared at `keyed.rs:20`, which this changeset renumbered
from the line it held at the base revision.

Reused: the helper the tail one follows is documented here for the first time,
and is declared at `keyed.rs:16` — the number that named the tail helper at the
base revision. The citation is correct and the rule reports it anyway.
