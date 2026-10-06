//! `command_socket_mode` end-to-end test: `sozu start` must give the command
//! socket the permission bits the configuration asks for, and keep the
//! historical `0600` when the key is absent.
//!
//! The mode used to be hard-coded to `0600`, so only the proxy's own user could
//! reach the socket and a monitoring agent running as another account could not
//! query it. Spawns the real `sozu` binary (`CARGO_BIN_EXE_sozu`) with one worker
//! and no listener, waits until the master answers `sozu status` over the
//! socket — the mode is applied right after `bind(2)` and before the command
//! server accepts — then reads the socket's mode.
//!
//! `#[ignore]`d so a contributor's `cargo test` stays fast; CI runs it from its
//! process-level e2e step (`.github/workflows/ci.yml`). Run it by hand with:
//!
//! ```bash
//! cargo test -p sozu --test command_socket_mode_e2e -- --ignored
//! ```
#![cfg(target_os = "linux")]

use std::{
    os::unix::fs::PermissionsExt,
    path::Path,
    process::{Child, Command, Stdio},
    thread,
    time::{Duration, Instant},
};

use nix::{
    sys::signal::{Signal, kill},
    unistd::Pid,
};

/// `true` once the master answers `sozu status` over its command socket.
fn master_answers(config_path: &Path) -> bool {
    Command::new(env!("CARGO_BIN_EXE_sozu"))
        .args(["-c", config_path.to_str().unwrap(), "status"])
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()
        .is_ok_and(|status| status.success())
}

fn stop(mut master: Child) {
    let _ = kill(Pid::from_raw(master.id() as i32), Signal::SIGTERM);
    let end = Instant::now() + Duration::from_secs(20);
    while Instant::now() < end {
        if master.try_wait().expect("try_wait").is_some() {
            return;
        }
        thread::sleep(Duration::from_millis(50));
    }
    let _ = master.kill();
    let _ = master.wait();
}

/// Start a master whose configuration carries `mode_line`, and return the
/// permission bits of the command socket once the master answers on it.
fn socket_mode_after_start(mode_line: &str) -> u32 {
    let temp = tempfile::tempdir().expect("tempdir");
    let socket_path = temp.path().join("sozu.sock");
    let config_path = temp.path().join("config.toml");

    let config = format!(
        r#"
command_socket = "{socket}"
{mode_line}
command_buffer_size = 16384
max_command_buffer_size = 163840
worker_count = 1
worker_automatic_restart = false
handle_process_affinity = false
log_level = "info"
log_target = "stderr"
max_connections = 100
activate_listeners = true
"#,
        socket = socket_path.display(),
    );
    std::fs::write(&config_path, &config).expect("write config");

    let master = Command::new(env!("CARGO_BIN_EXE_sozu"))
        .env_remove("RUST_LOG")
        .args(["start", "-c", config_path.to_str().unwrap()])
        .spawn()
        .expect("spawn sozu start");

    let ready_by = Instant::now() + Duration::from_secs(20);
    while !master_answers(&config_path) {
        if Instant::now() > ready_by {
            stop(master);
            panic!("the master never answered on {}", socket_path.display());
        }
        thread::sleep(Duration::from_millis(100));
    }

    let mode = std::fs::metadata(&socket_path)
        .expect("stat the command socket")
        .permissions()
        .mode()
        & 0o7777;
    stop(master);
    mode
}

#[test]
#[ignore = "process-level: spawns a real master and worker; run from the dedicated CI step or with --ignored (see module docs)"]
fn command_socket_gets_the_configured_mode() {
    let mode = socket_mode_after_start(r#"command_socket_mode = "0660""#);
    assert_eq!(
        mode, 0o660,
        "command socket mode is {mode:o}, the configuration asked for 660"
    );
}

#[test]
#[ignore = "process-level: spawns a real master and worker; run from the dedicated CI step or with --ignored (see module docs)"]
fn command_socket_keeps_0600_without_the_key() {
    let mode = socket_mode_after_start("");
    assert_eq!(
        mode, 0o600,
        "command socket mode is {mode:o}, the default must stay 600"
    );
}
