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

    // Git metadata for version display.
    // Falls back gracefully when building from a tarball without .git/.
    let commit_hash = git(&["rev-parse", "--short", "HEAD"]).unwrap_or_default();
    let commit_branch = git(&["rev-parse", "--abbrev-ref", "HEAD"]).unwrap_or_default();
    println!("cargo:rustc-env=SOZU_BUILD_GIT={commit_hash} {commit_branch}");

    // Build the +/- feature flags string, using raw Cargo feature names.
    // Each entry is (CARGO_FEATURE env var suffix, display name).
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

    // Rebuild when HEAD changes (commit, checkout, rebase).
    println!("cargo:rerun-if-changed=../.git/HEAD");
    println!("cargo:rerun-if-changed=../.git/refs/");
}

fn git(args: &[&str]) -> Option<String> {
    Command::new("git")
        .args(args)
        .output()
        .ok()
        .filter(|o| o.status.success())
        .map(|o| String::from_utf8_lossy(&o.stdout).trim().to_owned())
}
