//! Subprocess e2e tests for the `sozu top` TUI subcommand.
//!
//! These tests spawn the actual `sozu` binary (located via Cargo's
//! `CARGO_BIN_EXE_sozu` env var) and exercise the TUI's entry-point
//! behaviour — clap parsing, binary linkage, and (under `--ignored`) the
//! full transport-thread + render-loop lifecycle against a real Sōzu
//! master.
//!
//! The non-`#[ignore]`d tests run in CI and validate the bits that don't
//! depend on a running master: building the bin with `--features tui`,
//! invoking it without crashing, and producing the expected `--help`
//! shape. The full master-side e2e (`sozu_top_tick_once_against_real_master`)
//! is `#[ignore]`d so operators can run it manually with `cargo test
//! -p sozu --features tui --tests -- --ignored sozu_top` because it
//! spawns a daemonised master + writes to the filesystem.

#![cfg(feature = "tui")]

use std::process::Command;
use std::time::{Duration, Instant};

/// Path to the freshly-compiled `sozu` binary. Cargo populates this env
/// var for every integration test under `bin/tests/` so the binary the
/// test exercises is always the one the rest of the suite just built.
fn sozu_bin() -> &'static str {
    env!("CARGO_BIN_EXE_sozu")
}

/// `sozu top --help` must parse cleanly and print every flag added by
/// week-1's clap surface. Regression guard against accidentally dropping
/// a flag from `bin/src/cli.rs` or breaking the `#[cfg(feature = "tui")]`
/// gate on the `SubCmd::Top` variant.
#[test]
fn sozu_top_help_lists_every_flag() {
    let output = Command::new(sozu_bin())
        .args(["top", "--help"])
        .output()
        .expect("spawn sozu top --help");
    assert!(
        output.status.success(),
        "sozu top --help exited non-zero: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    let stdout = String::from_utf8_lossy(&output.stdout);
    for flag in [
        "--refresh-ms",
        "--no-color",
        "--no-mouse",
        "--skin",
        "--detail",
        "--lease-ttl-seconds",
        "--snapshot",
        "--tick-once",
        "--log-file",
        "--glyphs",
    ] {
        assert!(
            stdout.contains(flag),
            "sozu top --help missing flag `{flag}`. output:\n{stdout}",
        );
    }
}

/// `sozu --version` must report `+tui` when built with the feature, and
/// `-tui` otherwise. Build matrices that ship both variants rely on the
/// banner to tell them apart at deployment time. This test only runs
/// under `--features tui`, so it asserts the `+tui` side; the `-tui`
/// banner is covered by the default-feature snapshot of the version
/// string in CI's lean-build cell.
#[test]
fn sozu_version_reports_plus_tui() {
    let output = Command::new(sozu_bin())
        .arg("--version")
        .output()
        .expect("spawn sozu --version");
    assert!(output.status.success(), "sozu --version exited non-zero");
    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(
        stdout.contains("+tui"),
        "sozu --version did not list +tui under --features tui. output:\n{stdout}",
    );
    assert!(
        !stdout.contains("-tui"),
        "sozu --version listed -tui under --features tui. output:\n{stdout}",
    );
}

/// `sozu top --tick-once` against a real master: spawns a daemonised
/// `sozu start`, waits for the command socket to appear, runs the TUI
/// for exactly one frame, and asserts a clean exit. `#[ignore]`d by
/// default because it writes to a temp dir, binds an ephemeral port,
/// and depends on graceful master shutdown (a hung master would block
/// CI). Run manually with:
///
/// ```bash
/// cargo test -p sozu --features tui --tests -- --ignored sozu_top
/// ```
#[test]
#[ignore]
fn sozu_top_tick_once_against_real_master() {
    let temp = tempfile::tempdir().expect("tempdir");
    let socket_path = temp.path().join("sozu.sock");
    let config_path = temp.path().join("config.toml");

    // Minimum viable config: command socket only, no listeners, no
    // clusters, one worker, automatic-restart disabled (so the master
    // exits cleanly when we send SIGTERM).
    let config = format!(
        r#"
command_socket = "{socket}"
command_buffer_size = 16384
max_command_buffer_size = 163840
worker_count = 1
worker_automatic_restart = false
saved_state = ""
log_level = "warn"
log_target = "stderr"
max_connections = 100
buffer_size = 16393
"#,
        socket = socket_path.display(),
    );
    std::fs::write(&config_path, config).expect("write config");

    let mut master = Command::new(sozu_bin())
        .args(["start", "-c", config_path.to_str().unwrap()])
        .spawn()
        .expect("spawn sozu start");

    // Wait up to 10 s for the master to create the command socket.
    let deadline = Instant::now() + Duration::from_secs(10);
    while !socket_path.exists() {
        if Instant::now() > deadline {
            let _ = master.kill();
            let _ = master.wait();
            panic!("sozu master never created {socket_path:?}");
        }
        std::thread::sleep(Duration::from_millis(50));
    }

    // Now exercise the TUI for a single tick. `--no-mouse` skips SGR
    // mouse capture (avoids stale escape sequences in the parent shell
    // if the test harness leaks them); `--snapshot 1` renders one
    // frame and exits.
    let output = Command::new(sozu_bin())
        .args([
            "-c",
            config_path.to_str().unwrap(),
            "top",
            "--snapshot",
            "1",
            "--no-mouse",
            "--no-color",
        ])
        .output()
        .expect("spawn sozu top --snapshot 1");

    // Send SIGTERM to the master; SoftStop drains and exits. Give it 5 s.
    let _ = master.kill();
    let _ = master.wait();

    assert!(
        output.status.success(),
        "sozu top --snapshot 1 exited non-zero: stderr=\n{}",
        String::from_utf8_lossy(&output.stderr)
    );
}
