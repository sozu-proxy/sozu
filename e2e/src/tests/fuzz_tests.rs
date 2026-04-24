//! Integration test that runs fuzz targets for a bounded time.
//! Requires: cargo-fuzz installed, nightly Rust toolchain.
//!
//! Both targets run 10 s of fuzzing each and are part of the default
//! `cargo test` run. Developers without nightly + cargo-fuzz will see
//! them fail — install with `rustup toolchain install nightly` and
//! `cargo install cargo-fuzz`.

#[test]
fn fuzz_frame_parser() {
    let status = std::process::Command::new("cargo")
        .args([
            "+nightly",
            "fuzz",
            "run",
            "fuzz_frame_parser",
            "--",
            "-max_total_time=10",
        ])
        .current_dir(concat!(env!("CARGO_MANIFEST_DIR"), "/../fuzz"))
        .status()
        .expect("failed to run cargo fuzz — is cargo-fuzz installed?");
    assert!(status.success(), "fuzz_frame_parser exited with failure");
}

#[test]
fn fuzz_hpack_decoder() {
    let status = std::process::Command::new("cargo")
        .args([
            "+nightly",
            "fuzz",
            "run",
            "fuzz_hpack_decoder",
            "--",
            "-max_total_time=10",
        ])
        .current_dir(concat!(env!("CARGO_MANIFEST_DIR"), "/../fuzz"))
        .status()
        .expect("failed to run cargo fuzz — is cargo-fuzz installed?");
    assert!(status.success(), "fuzz_hpack_decoder exited with failure");
}
