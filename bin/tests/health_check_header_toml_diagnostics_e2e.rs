//! A malformed configuration file must never leak a health-check header value.
//!
//! Spawns the real `sozu` binary (`CARGO_BIN_EXE_sozu`) on `config check` with
//! a file whose parse fails, and asserts that a synthetic credential written in
//! `[[clusters.<id>.health_check.headers]]` reaches neither stdout nor stderr.
//! The diagnostic keeps the config path and a byte range (see
//! `doc/testing.md`, regression guards).

use std::process::Command;

const SENTINEL: &str = "SYNTHETIC-HC-SECRET-91c2e4";

/// Run `sozu -c <file> config check` on `toml`; return (success, stdout, stderr).
fn check(toml: &str) -> (bool, String, String) {
    let dir = tempfile::tempdir().expect("tempdir");
    let path = dir.path().join("config.toml");
    std::fs::write(&path, toml).expect("write config");
    let output = Command::new(env!("CARGO_BIN_EXE_sozu"))
        .args(["-c", path.to_str().expect("utf-8 path"), "config", "check"])
        .output()
        .expect("spawn sozu");
    (
        output.status.success(),
        String::from_utf8_lossy(&output.stdout).into_owned(),
        String::from_utf8_lossy(&output.stderr).into_owned(),
    )
}

fn assert_no_leak(success: bool, stdout: &str, stderr: &str) {
    assert!(!success, "a malformed config must exit nonzero");
    assert!(
        !stdout.contains(SENTINEL),
        "stdout leaked the value: {stdout}"
    );
    assert!(
        !stderr.contains(SENTINEL),
        "stderr leaked the value: {stderr}"
    );
    assert!(
        stdout.contains("config.toml") || stderr.contains("config.toml"),
        "the diagnostic keeps the config path: {stdout} {stderr}"
    );
}

/// The syntax error sits far from the header: the whole source must not be
/// echoed.
#[test]
fn a_parse_error_elsewhere_does_not_echo_a_header_value() {
    let toml = format!(
        "[clusters.app]\nprotocol = \"http\"\nfrontends = []\n\n\
         [clusters.app.health_check]\nuri = \"/\"\n\n\
         [[clusters.app.health_check.headers]]\nname = \"Authorization\"\nvalue = \"{SENTINEL}\"\n\n\
         this line is not toml\n"
    );
    let (success, stdout, stderr) = check(&toml);
    assert_no_leak(success, &stdout, &stderr);
}

/// The syntax error is on the header line itself: the `toml` excerpt of that
/// line must not be echoed either.
#[test]
fn a_parse_error_on_the_header_line_does_not_echo_its_value() {
    let toml = format!(
        "[clusters.app]\nprotocol = \"http\"\nfrontends = []\n\n\
         [clusters.app.health_check]\nuri = \"/\"\n\n\
         [[clusters.app.health_check.headers]]\nname = \"Authorization\"\nvalue = \"{SENTINEL}\" trailing\n"
    );
    let (success, stdout, stderr) = check(&toml);
    assert_no_leak(success, &stdout, &stderr);
}
