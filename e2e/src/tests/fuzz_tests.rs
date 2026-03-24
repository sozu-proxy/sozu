//! Integration test that runs fuzz targets for a bounded time.
//! Requires: cargo-fuzz installed, nightly Rust toolchain.
//! Run manually: cargo test -p sozu-e2e -- --ignored fuzz

#[test]
#[ignore] // requires nightly + cargo-fuzz
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
#[ignore] // requires nightly + cargo-fuzz
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
