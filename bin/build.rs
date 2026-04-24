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

    let git_sha = Command::new("git")
        .args(["rev-parse", "--short=12", "HEAD"])
        .output()
        .ok()
        .filter(|out| out.status.success())
        .map(|out| String::from_utf8_lossy(&out.stdout).trim().to_owned())
        .filter(|s| !s.is_empty())
        .unwrap_or_else(|| "unknown".to_owned());
    println!("cargo:rustc-env=SOZU_BUILD_GIT_SHA={git_sha}");
}
