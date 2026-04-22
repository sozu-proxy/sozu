# Nightly CI matrix — allowed-to-fail notes

The CI matrix (`ci.yml`) includes `rust: nightly` with `experimental: true` / `continue-on-error: true`, making it non-blocking for PR merges.

## Current status (2026-04-22)

**Classification: no nightly-specific failure observed.**

Audit of all completed `feat/h2-mux` CI runs (runs 24682798916 through 24768926470):
every `Test (nightly, true)` job that reached completion returned **success**. The one
historical `feat/h2-mux` failure (run 24718201148) was a stable job failure, not nightly.

The codebase contains no `#![feature(...)]` gates, so it is fully stable-compatible and
nightly offers no additional risk surface beyond compiler internals.

## Why nightly is allowed-to-fail

Nightly rustc occasionally introduces new lints, renames unstable flags, or changes
type-inference behaviour in ways that break otherwise-correct code. The `experimental: true`
row exists to surface such regressions early without blocking stable CI. As of this writing
the nightly job passes cleanly and requires no suppression attributes.

## Action required

None. Re-check if a future nightly job starts failing consistently; at that point inspect
with `gh run view <run-id> --log-failed` and add a targeted `#[allow(...)]` or open a
rustc issue as appropriate.
