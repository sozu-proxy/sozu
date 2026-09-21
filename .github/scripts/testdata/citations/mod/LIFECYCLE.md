# Module fixture — the `**/LIFECYCLE.md` half of the guarded surface

This document exists so that dropping `**/LIFECYCLE.md` from the scanned
surface, or dropping the digit from the extraction pattern, fails the
self-test instead of passing quietly.

A module document cites its own siblings by bare name: `Connection` is
declared at `h2.rs:5` and `Connection::readable` returns at `h2.rs:8`.
