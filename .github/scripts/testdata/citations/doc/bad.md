# Broken fixture — every citation here must be reported

Past end of file: `sample.rs:99`.

Blank line: `sample.rs:4`.

Missing file: `nowhere.rs:1`.

Half-renumbered slash continuation: `sample.rs:3/3`, continued bare at `:99`.

Line zero: `sample.rs:0`.

Range ending on a blank line: `sample.rs:3-5`.

Digit-bearing basename, past end of file: `h2.rs:99`.

Markdown target, past end of file: `reference.md:99`.

Markdown target, blank line: `doc/reference.md:10`.

Two citations on one line, so the continuation must inherit from the NEAREST
one and not the first: `h2.rs:1` and `sample.rs:3`, continued bare at `:99`.

Bare GROUP continuation, renumbered on one half only: `sample.rs:3` and `:3/3`.

Bare group with no citation earlier on its own line: `:1/2`.

A continuation inherits on its own line only, so this one inherits nothing
even though `sample.rs:3` sits in the same paragraph, one line above it:
`:7`.

Bare continuation with no citation earlier on its own line, and none in its
paragraph either: `:7`.
