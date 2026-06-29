//! `sozu-sim` — home for moonpool-driven deterministic simulations of Sōzu's
//! sans-io cores.
//!
//! The harness itself lives in `tests/udp_simulation.rs`. It is test-only and
//! pulls the async moonpool / tokio closure (which must never enter `sozu-lib`,
//! per the no-`async fn`-in-`lib/` rule), so this library deliberately has no
//! runtime surface. See `doc/udp_simulation.md` and `doc/testing.md`.
