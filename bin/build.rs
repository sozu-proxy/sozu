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

    // Crypto-provider features follow the runtime precedence chain
    // implemented in `lib/src/crypto.rs::default_provider()`:
    // `fips > ring > aws-lc-rs > openssl`. Several can be active in the
    // same build (the canonical case being `cargo build --features fips`
    // with the default `crypto-ring` still active), but at runtime only
    // one provider is wired into rustls. Mirror that in the `--version`
    // banner: report the effective provider as `+` and force the losers
    // to `-`, so operators reading the banner see the provider they will
    // actually be talking to instead of the literal Cargo feature set.
    //
    // `fips` is a build-mode layered on top of `crypto-aws-lc-rs` (per
    // `lib/Cargo.toml: fips = ["crypto-aws-lc-rs", "rustls/fips"]`), so
    // when `fips` wins the chain `crypto-aws-lc-rs` is also reported as
    // `+` — aws-lc-rs is genuinely the underlying library, just driven
    // through rustls's FIPS profile. The other two providers stay `-`.
    let active = |suffix: &str| -> bool { env::var(format!("CARGO_FEATURE_{suffix}")).is_ok() };
    let crypto_effective: &[&str] = if active("FIPS") {
        &["FIPS", "CRYPTO_AWS_LC_RS"]
    } else if active("CRYPTO_RING") {
        &["CRYPTO_RING"]
    } else if active("CRYPTO_AWS_LC_RS") {
        &["CRYPTO_AWS_LC_RS"]
    } else if active("CRYPTO_OPENSSL") {
        &["CRYPTO_OPENSSL"]
    } else {
        &[]
    };
    const CRYPTO_PROVIDER_SUFFIXES: &[&str] =
        &["CRYPTO_RING", "CRYPTO_AWS_LC_RS", "CRYPTO_OPENSSL", "FIPS"];

    let flags: Vec<String> = features
        .iter()
        .map(|(env_suffix, display)| {
            let on = if CRYPTO_PROVIDER_SUFFIXES.contains(env_suffix) {
                crypto_effective.contains(env_suffix)
            } else {
                active(env_suffix)
            };
            if on {
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
