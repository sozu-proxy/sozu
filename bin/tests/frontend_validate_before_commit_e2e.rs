//! sozu#1313 end-to-end smoke test.
//!
//! A saved state file carrying a frontend whose hostname the workers' router
//! refuses (the production incident: a trailing `/` with no openable regex
//! segment) must NOT re-enter the main process's `ConfigState` when it is
//! replayed. The replay path has no rollback whatsoever, so before the fix the
//! entry was dispatched into the master's state before the fan-out, survived
//! the workers' unanimous rejection, and was written straight back out by the
//! next `SaveState` — poisoning every subsequent replay.
//!
//! The poisoned file is produced with the PRODUCTION serialiser
//! (`ConfigState::write_requests_to_file`), not a hand-written fixture, so the
//! test exercises exactly the bytes `sozu state save` would have written on the
//! affected instance.
//!
//! Spawns the real `sozu` binary (`CARGO_BIN_EXE_sozu`) as a daemonised master
//! and drives it over its unix command socket with `sozu state load` /
//! `sozu state save`.
//!
//! `#[ignore]`d by default because it spawns a master and writes to a temp dir.
//! Run manually with:
//!
//! ```bash
//! cargo test -p sozu --test frontend_validate_before_commit_e2e -- --ignored
//! ```

use std::collections::BTreeMap;
use std::process::Command;
use std::time::{Duration, Instant};

use sozu_command_lib::proto::command::{
    PathRule, PathRuleKind, RequestHttpFrontend, RulePosition, SocketAddress, request::RequestType,
};
use sozu_command_lib::state::ConfigState;

/// The hostname that panicked every worker in production.
const INCIDENT_HOSTNAME: &str = "raat-app.cleverapps.io/";

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
#[ignore = "manual: spawns a real master over a unix socket; run with --ignored (see module docs)"]
fn replaying_a_poisoned_state_file_does_not_re_persist_the_frontend() {
    let temp = tempfile::tempdir().expect("tempdir");
    let socket_path = temp.path().join("sozu.sock");
    let config_path = temp.path().join("config.toml");
    let poisoned_path = temp.path().join("poisoned.state");
    let saved_path = temp.path().join("out.state");

    // 1) Build the poisoned state file the way the affected instance did:
    //    `ConfigState` accepts the malformed frontend (it has no route-grammar
    //    check — see `frontend_validation_tests` in bin/src/command/requests.rs)
    //    and `write_requests_to_file` is the very serialiser `state save` uses.
    {
        let mut state = ConfigState::new();
        let front = RequestHttpFrontend {
            cluster_id: Some("poisoned-cluster".to_owned()),
            address: SocketAddress::new_v4(127, 0, 0, 1, free_port()),
            hostname: INCIDENT_HOSTNAME.to_owned(),
            path: PathRule {
                kind: PathRuleKind::Prefix as i32,
                value: "/".to_owned(),
            },
            method: None,
            position: RulePosition::Tree as i32,
            tags: BTreeMap::new(),
            redirect: None,
            redirect_scheme: None,
            redirect_template: None,
            rewrite_host: None,
            rewrite_path: None,
            rewrite_port: None,
            required_auth: None,
            headers: Vec::new(),
            hsts: None,
        };
        state
            .dispatch(&RequestType::AddHttpFrontend(front).into())
            .expect("ConfigState records the malformed frontend — it has no route-grammar check");
        let mut file = std::fs::File::create(&poisoned_path).expect("create poisoned state file");
        let written = state
            .write_requests_to_file(&mut file)
            .expect("serialise the poisoned state");
        assert_eq!(
            written, 1,
            "the poisoned state must hold exactly one request"
        );
        let bytes = std::fs::read(&poisoned_path).expect("read back the poisoned state");
        assert!(
            String::from_utf8_lossy(&bytes).contains(INCIDENT_HOSTNAME),
            "the fixture must actually carry the malformed hostname"
        );
    }

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

    let cfg = config_path.to_str().unwrap();

    // 2) Replay the poisoned file. THE #1313 FIX: the entry is skipped before
    //    `ConfigState::dispatch`, so the load succeeds (fail-open per entry) and
    //    nothing is fanned out. Pre-fix the entry was committed and every worker
    //    rejected it, failing the load AND leaving the state poisoned.
    let load = Command::new(sozu_bin())
        .args([
            "-c",
            cfg,
            "state",
            "load",
            "-f",
            poisoned_path.to_str().unwrap(),
        ])
        .output()
        .expect("spawn `state load`");
    let load_ok = load.status.success();

    // 3) Save the state back out and look for the malformed hostname in it.
    let save = Command::new(sozu_bin())
        .args([
            "-c",
            cfg,
            "state",
            "save",
            "-f",
            saved_path.to_str().unwrap(),
        ])
        .output()
        .expect("spawn `state save`");
    let save_ok = save.status.success();
    let saved = std::fs::read(&saved_path).unwrap_or_default();
    let saved = String::from_utf8_lossy(&saved).into_owned();

    // Clean up the master before asserting (a failed assert must not leak it).
    let _ = master.kill();
    let _ = master.wait();

    assert!(
        save_ok,
        "`state save` must succeed.\nstdout=\n{}\nstderr=\n{}",
        String::from_utf8_lossy(&save.stdout),
        String::from_utf8_lossy(&save.stderr),
    );
    // The load-bearing assertion: the poisoned entry never entered the main
    // process's state, so it cannot be re-persisted — and cannot poison the
    // next replay either.
    assert!(
        !saved.contains(INCIDENT_HOSTNAME),
        "a frontend rejected by the router must not be committed by the replay \
         and re-persisted by SaveState (sozu#1313); saved state was:\n{saved}",
    );
    assert!(
        load_ok,
        "`state load` must skip the invalid entry without aborting the load \
         (sozu#1313).\nstdout=\n{}\nstderr=\n{}",
        String::from_utf8_lossy(&load.stdout),
        String::from_utf8_lossy(&load.stderr),
    );
}
