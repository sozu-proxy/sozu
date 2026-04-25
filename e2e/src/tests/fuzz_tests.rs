//! Integration test that runs fuzz targets for a bounded time.
//! Requires: cargo-fuzz installed, nightly Rust toolchain.
//!
//! Both targets run 10 s of fuzzing each and are part of the default
//! `cargo test` run. When the nightly toolchain or `cargo-fuzz` is not
//! available — common on developer machines without `rustup` nightly
//! and on CI legs that lack the install step — the tests log a notice
//! and return cleanly rather than panicking, so the rest of the e2e
//! suite still runs. To enable the conformance gate locally, install
//! the prerequisites with `rustup toolchain install nightly` and
//! `cargo install cargo-fuzz`.

use std::process::Command;

/// Probe whether the prerequisites (nightly toolchain + cargo-fuzz) are
/// available. Returns `Some(reason)` when the test should be skipped, with
/// a one-line human description of which prerequisite is missing.
fn fuzz_prereqs_missing() -> Option<&'static str> {
    if Command::new("rustup")
        .args(["which", "--toolchain", "nightly", "cargo"])
        .output()
        .ok()
        .filter(|o| o.status.success())
        .is_none()
    {
        return Some("nightly toolchain not installed — `rustup toolchain install nightly`");
    }
    if Command::new("cargo")
        .args(["+nightly", "fuzz", "--version"])
        .output()
        .ok()
        .filter(|o| o.status.success())
        .is_none()
    {
        return Some("cargo-fuzz not installed — `cargo install cargo-fuzz`");
    }
    None
}

/// Run a single fuzz target for `max_total_time=10` seconds. Skips
/// gracefully (with a stderr notice) when prerequisites are missing.
fn run_fuzz_target(target: &str) {
    if let Some(reason) = fuzz_prereqs_missing() {
        eprintln!("{target}: skipping fuzz run — {reason}");
        return;
    }
    let status = Command::new("cargo")
        .args([
            "+nightly",
            "fuzz",
            "run",
            target,
            "--",
            "-max_total_time=10",
        ])
        .current_dir(concat!(env!("CARGO_MANIFEST_DIR"), "/../fuzz"))
        .status()
        .expect("failed to spawn cargo fuzz despite prereq probe — environment race?");
    assert!(status.success(), "{target} exited with failure");
}

#[test]
fn fuzz_frame_parser() {
    run_fuzz_target("fuzz_frame_parser");
}

#[test]
fn fuzz_hpack_decoder() {
    run_fuzz_target("fuzz_hpack_decoder");
}
