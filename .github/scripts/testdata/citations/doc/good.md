# Citation fixture — every citation here must resolve

`Sample::readable` is at `sample.rs:6`, and its body spans `sample.rs:6-8`.

The struct and the impl block are at `sample.rs:3/6`, and the comma form
`sample.rs:1, 10` resolves both halves.

A continuation may wrap the prose line (`sample.rs:3,
6`) and must still resolve.

A markdown document is a citation target under the same rules: `doc/reference.md:5`
is repo-root-relative, the bare range `reference.md:3-5` binds to this document's
own directory, and the multi-target form `doc/reference.md:5, 7, 9` resolves every span.

The bare `LIFECYCLE.md:8` resolves through neither of those two branches: no
sibling of this document carries that name and neither does the fixture root, so
it reaches the tree-wide suffix index and binds to the one `LIFECYCLE.md` there.
It is the only fixture citation that does, which makes it the only one that
fails when `.md` leaves TARGET_SUFFIXES.
