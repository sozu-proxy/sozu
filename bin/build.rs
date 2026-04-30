use std::{env, process::Command};

fn main() {
    // Export defaults as compile time environment variables.
    // These variables are to be set by package managers in their build script.
    // `export SOZU_CONFIG=<default configuration file> && cargo build`.
    let variables = vec!["SOZU_CONFIG", "SOZU_PID_FILE_PATH"];
    for variable in variables {
        if let Ok(val) = env::var(variable) {
            println!("cargo:rustc-env={variable}={val}");
        }
    }

    // Embed the short git SHA into the binary so the audit log can render
    // `build_git_sha=<short>` and operators can pin a line back to an exact
    // commit across mixed-fleet upgrades. Re-run only when HEAD or refs
    // move so cached `cargo build` invocations stay fast.
    println!("cargo:rerun-if-changed=build.rs");
    println!("cargo:rerun-if-changed=../.git/HEAD");
    println!("cargo:rerun-if-changed=../.git/refs");

    let git_sha = git_output(&["rev-parse", "--short=12", "HEAD"])
        .filter(|s| !s.is_empty())
        .unwrap_or_else(|| "unknown".to_owned());
    println!("cargo:rustc-env=SOZU_BUILD_GIT_SHA={git_sha}");

    // Git metadata for the systemd-style `--version` banner. Captures
    // shorter SHA + branch separately so `sozu --version` can render
    // both. Falls back to empty strings when building from a tarball
    // without .git/ (we still ship a usable binary).
    let commit_hash = git_output(&["rev-parse", "--short", "HEAD"]).unwrap_or_default();
    let commit_branch = git_output(&["rev-parse", "--abbrev-ref", "HEAD"]).unwrap_or_default();
    println!("cargo:rustc-env=SOZU_BUILD_GIT={commit_hash} {commit_branch}");

    // Build the +/- feature flags string, using raw Cargo feature names.
    // Each entry is (CARGO_FEATURE env var suffix, display name). The
    // `--version` handler in `cli.rs` decides at runtime whether to wrap
    // each entry in ANSI escapes; build.rs emits raw text only.
    let features: &[(&str, &str)] = &[
        ("JEMALLOCATOR", "jemallocator"),
        ("CRYPTO_RING", "crypto-ring"),
        ("CRYPTO_AWS_LC_RS", "crypto-aws-lc-rs"),
        ("CRYPTO_OPENSSL", "crypto-openssl"),
        ("FIPS", "fips"),
        ("SIMD", "simd"),
        ("SPLICE", "splice"),
        ("OPENTELEMETRY", "opentelemetry"),
        ("TOLERANT_HTTP1_PARSER", "tolerant-http1-parser"),
        ("LOGS_DEBUG", "logs-debug"),
        ("LOGS_TRACE", "logs-trace"),
        ("UNSTABLE", "unstable"),
    ];

    let flags: Vec<String> = features
        .iter()
        .map(|(env_suffix, display)| {
            let key = format!("CARGO_FEATURE_{env_suffix}");
            if env::var(&key).is_ok() {
                format!("+{display}")
            } else {
                format!("-{display}")
            }
        })
        .collect();

    println!("cargo:rustc-env=SOZU_BUILD_FEATURES={}", flags.join(" "));
}

fn git_output(args: &[&str]) -> Option<String> {
    Command::new("git")
        .args(args)
        .output()
        .ok()
        .filter(|o| o.status.success())
        .map(|o| String::from_utf8_lossy(&o.stdout).trim().to_owned())
}
