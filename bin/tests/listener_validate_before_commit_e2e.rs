//! sozu#1301 end-to-end smoke test.
//!
//! An HTTPS listener whose configuration the worker cannot build must NOT
//! reserve its address in the main process's `ConfigState`, so a corrected
//! `listener https add` at the same address still succeeds. Before the fix the
//! invalid listener was committed (and the address reserved) before the worker
//! rejected it, and the corrected reload was then refused with
//! `StateError::Exists`.
//!
//! Spawns the real `sozu` binary (`CARGO_BIN_EXE_sozu`) as a daemonised master
//! and drives it over its unix command socket with the `sozu listener https
//! add` CLI. The unbuildable listener is produced with a malformed `--answer-404`
//! template file — a deterministic, crypto-provider-independent trigger (it
//! fails `HttpAnswers::new`, not the rustls build).
//!
//! `#[ignore]`d by default because it spawns a master, writes to a temp dir and
//! binds an ephemeral port. Run manually with:
//!
//! ```bash
//! cargo test -p sozu --test listener_validate_before_commit_e2e -- --ignored
//! ```

use std::process::Command;
use std::time::{Duration, Instant};

fn sozu_bin() -> &'static str {
    env!("CARGO_BIN_EXE_sozu")
}

/// Grab a currently-free 127.0.0.1 TCP port by binding to `:0` and releasing
/// it. Racy in principle, fine for a manual `#[ignore]`d test.
fn free_port() -> u16 {
    std::net::TcpListener::bind("127.0.0.1:0")
        .expect("bind ephemeral port")
        .local_addr()
        .expect("local_addr")
        .port()
}

#[test]
#[ignore = "manual: spawns a real master over a unix socket + binds a port; run with --ignored (see module docs)"]
fn corrected_https_listener_add_succeeds_after_invalid_one() {
    let temp = tempfile::tempdir().expect("tempdir");
    let socket_path = temp.path().join("sozu.sock");
    let config_path = temp.path().join("config.toml");
    let bad_answer = temp.path().join("bad_404.txt");
    std::fs::write(&bad_answer, "not a valid http response").expect("write bad answer template");

    // Minimum viable config: command socket only, no listeners, one worker,
    // automatic-restart disabled so the master exits cleanly on kill.
    let config = format!(
        r#"
command_socket = "{socket}"
command_buffer_size = 16384
max_command_buffer_size = 163840
worker_count = 1
worker_automatic_restart = false
log_level = "warn"
log_target = "stderr"
max_connections = 100
buffer_size = 16393
"#,
        socket = socket_path.display(),
    );
    std::fs::write(&config_path, &config).expect("write config");

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

    let address = format!("127.0.0.1:{}", free_port());
    let cfg = config_path.to_str().unwrap();
    let bad_answer_path = bad_answer.to_str().unwrap();
    // A bare `listener https add` leaves the TLS version set empty, which
    // rustls rejects on its own ("no usable cipher suites"); pass a real
    // version set so the ONLY invalid thing about the first add is its answer
    // template, and the corrected add is genuinely buildable.
    let tls = ["--tls-versions", "TLS_V12", "--tls-versions", "TLS_V13"];

    // 1) An unbuildable HTTPS listener (malformed 404 answer template) is
    //    rejected. Pre-fix it was rejected too, but only AFTER the main process
    //    had already committed it and reserved the address.
    let invalid = Command::new(sozu_bin())
        .args(["-c", cfg, "listener", "https", "add", "-a", &address])
        .args(tls)
        .args(["--answer-404", bad_answer_path])
        .output()
        .expect("spawn invalid `listener https add`");
    let invalid_ok = invalid.status.success();

    // 2) THE #1301 FIX: a corrected add at the SAME address now succeeds,
    //    because the rejected listener never reserved the address. Before the
    //    fix this was refused with StateError::Exists.
    let corrected = Command::new(sozu_bin())
        .args(["-c", cfg, "listener", "https", "add", "-a", &address])
        .args(tls)
        .output()
        .expect("spawn corrected `listener https add`");
    let corrected_ok = corrected.status.success();

    // Clean up the master before asserting (a failed assert must not leak it).
    let _ = master.kill();
    let _ = master.wait();

    assert!(
        !invalid_ok,
        "an unbuildable HTTPS listener add should fail.\nstdout=\n{}\nstderr=\n{}",
        String::from_utf8_lossy(&invalid.stdout),
        String::from_utf8_lossy(&invalid.stderr),
    );
    assert!(
        corrected_ok,
        "corrected HTTPS listener add at the same address must succeed (sozu#1301) — \
         it was blocked, which means the invalid listener reserved the address.\n\
         stdout=\n{}\nstderr=\n{}",
        String::from_utf8_lossy(&corrected.stdout),
        String::from_utf8_lossy(&corrected.stderr),
    );
}
