//! sozu#1313 end-to-end smoke test: the replay rollback.
//!
//! A saved state file carrying an entry the main process accepts but every
//! worker REJECTS must not survive the replay. Before the fix `LoadStateTask`
//! only tallied the fleet-wide ok/errors of a `DefaultGatherer` and never
//! touched `server.state`: the entry stayed in the master's `ConfigState`, the
//! next `SaveState` wrote it straight back out, and the next replay re-injected
//! it — the poisoned-state loop of the incident.
//!
//! The entry used here is an `AddHttpFrontend` on an address that has NO HTTP
//! listener. It is well-formed, so it passes the master's pre-dispatch
//! `validate_request` (sozu#1301/#1313 guards) and is committed to
//! `ConfigState`; the worker refuses it with `ProxyError::NoListenerFound`
//! (`lib/src/http.rs:1151`) and answers `Failure`. That is exactly the shape
//! the per-entry rollback must catch: unanimous rejection of ONE entry inside a
//! bulk replay. A malformed certificate would also be refused by the worker,
//! but `compute_rollback` has no unambiguous inverse for `AddCertificate`, so
//! it could not prove anything here.
//!
//! The poisoned file is produced with the PRODUCTION serialiser
//! (`ConfigState::write_requests_to_file`), not a hand-written fixture, so the
//! test exercises exactly the bytes `sozu state save` would have written.
//!
//! The test also bounds the wall-clock duration of `sozu state load`: the
//! second half of the incident was the load never answering at all, because the
//! task scattered with `Timeout::None` and one unanswerable request left
//! `ok + errors` permanently short of `expected_responses`.
//!
//! `#[ignore]`d by default because it spawns a master and writes to a temp dir.
//! Run manually with:
//!
//! ```bash
//! cargo test -p sozu --test load_state_rollback_e2e -- --ignored
//! ```

use std::collections::BTreeMap;
use std::process::{Child, Command, Output, Stdio};
use std::time::{Duration, Instant};

use sozu_command_lib::proto::command::{
    PathRule, PathRuleKind, RequestHttpFrontend, RulePosition, SocketAddress, request::RequestType,
};
use sozu_command_lib::state::ConfigState;

/// A perfectly well-formed hostname: the master must ACCEPT this entry (that is
/// the point — the rollback, not the pre-dispatch validation, is under test).
const REJECTED_HOSTNAME: &str = "no-listener.example.com";

/// Upper bound on every `sozu` CLI invocation this test makes. The master's own
/// deadline for a one-entry replay is `worker_timeout` (5 s below) and the CLI
/// is allowed 30 s client-side, so a master that hangs — the second half of
/// sozu#1313 — shows up as a hang rather than as a fast client-side timeout.
/// Enforced by [`run_bounded`]: a genuine hang must FAIL this test, never block
/// CI forever.
const CLI_DEADLINE: Duration = Duration::from_secs(25);

/// The spawned master, reaped in `drop`.
///
/// Every exit from the test — a clean return, a failed assertion, a `panic!`
/// inside a helper — unwinds through this guard, so the process is killed
/// instead of being leaked into the CI runner. Manual `kill`/`wait` calls at
/// each panic site would only cover the paths someone remembered.
struct MasterProcess(Child);

impl Drop for MasterProcess {
    fn drop(&mut self) {
        let _ = self.0.kill();
        let _ = self.0.wait();
    }
}

/// Run one `sozu` CLI invocation under a hard deadline: on expiry the CLI child
/// is killed and the test fails (the master is reaped by [`MasterProcess`] as
/// the panic unwinds).
///
/// Output is piped and read once the child has exited. Every invocation here
/// prints a handful of lines, well under the pipe capacity, so polling without
/// draining cannot deadlock the child.
fn run_bounded(args: &[&str]) -> Output {
    let mut child = Command::new(sozu_bin())
        .args(args)
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap_or_else(|e| panic!("spawn `sozu {}`: {e}", args.join(" ")));
    let started_at = Instant::now();
    loop {
        match child.try_wait().expect("try_wait") {
            Some(_) => return child.wait_with_output().expect("wait_with_output"),
            None => {
                if started_at.elapsed() > CLI_DEADLINE {
                    let _ = child.kill();
                    let _ = child.wait();
                    panic!(
                        "`sozu {}` did not answer within {CLI_DEADLINE:?} — the master is hung \
                         (sozu#1313)",
                        args.join(" ")
                    );
                }
                std::thread::sleep(Duration::from_millis(50));
            }
        }
    }
}

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
#[ignore = "manual: spawns a real master and a worker over a unix socket; run with --ignored (see module docs)"]
fn a_replayed_entry_every_worker_rejects_is_not_re_persisted() {
    let temp = tempfile::tempdir().expect("tempdir");
    let socket_path = temp.path().join("sozu.sock");
    let config_path = temp.path().join("config.toml");
    let poisoned_path = temp.path().join("poisoned.state");
    let saved_path = temp.path().join("out.state");
    let frontend_port = free_port();

    // 1) Build the state file the way `sozu state save` would have.
    {
        let mut state = ConfigState::new();
        let front = RequestHttpFrontend {
            cluster_id: Some("rejected-cluster".to_owned()),
            address: SocketAddress::new_v4(127, 0, 0, 1, frontend_port),
            hostname: REJECTED_HOSTNAME.to_owned(),
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
            .expect("ConfigState records a well-formed frontend");
        let mut file = std::fs::File::create(&poisoned_path).expect("create poisoned state file");
        let written = state
            .write_requests_to_file(&mut file)
            .expect("serialise the poisoned state");
        assert_eq!(
            written, 1,
            "the poisoned state must hold exactly one request"
        );
    }

    // Minimum viable config: command socket only, NO listener at all — which is
    // precisely why the worker refuses the frontend above.
    let config = format!(
        r#"
command_socket = "{socket}"
command_buffer_size = 16384
max_command_buffer_size = 163840
worker_count = 1
worker_automatic_restart = false
worker_timeout = 5
ctl_command_timeout = 30000
log_level = "warn"
log_target = "stderr"
max_connections = 100
buffer_size = 16393
"#,
        socket = socket_path.display(),
    );
    std::fs::write(&config_path, &config).expect("write config");

    let _master = MasterProcess(
        Command::new(sozu_bin())
            .args(["start", "-c", config_path.to_str().unwrap()])
            .spawn()
            .expect("spawn sozu start"),
    );

    let cfg = config_path.to_str().unwrap().to_owned();

    // Wait up to 10 s for the master to create the command socket.
    let deadline = Instant::now() + Duration::from_secs(10);
    while !socket_path.exists() {
        if Instant::now() > deadline {
            panic!("sozu master never created {socket_path:?}");
        }
        std::thread::sleep(Duration::from_millis(50));
    }

    // …and until the worker actually answers: with zero live workers the
    // fan-out expects nothing, and an entry nobody was asked about is never
    // rolled back (`should_rollback_fanout` requires `expected > 0`). Without
    // this wait the test could pass for the wrong reason, or fail spuriously.
    let deadline = Instant::now() + Duration::from_secs(10);
    loop {
        let status = run_bounded(&["-c", &cfg, "status"]);
        if status.status.success() {
            break;
        }
        if Instant::now() > deadline {
            panic!(
                "no worker answered `status` in time:\nstdout=\n{}\nstderr=\n{}",
                String::from_utf8_lossy(&status.stdout),
                String::from_utf8_lossy(&status.stderr),
            );
        }
        std::thread::sleep(Duration::from_millis(100));
    }

    // 2) Replay the file. The master commits the entry, the worker refuses it,
    //    and THE #1313 FIX reverts that one entry from the master's state.
    let started_at = Instant::now();
    let load = run_bounded(&[
        "-c",
        &cfg,
        "state",
        "load",
        "-f",
        poisoned_path.to_str().unwrap(),
    ]);
    let load_elapsed = started_at.elapsed();
    let load_stdout = String::from_utf8_lossy(&load.stdout).into_owned();
    let load_stderr = String::from_utf8_lossy(&load.stderr).into_owned();

    // 3) Save the state back out and look for the rejected entry in it.
    let save = run_bounded(&[
        "-c",
        &cfg,
        "state",
        "save",
        "-f",
        saved_path.to_str().unwrap(),
    ]);
    let save_ok = save.status.success();
    let saved = std::fs::read(&saved_path).unwrap_or_default();
    let saved = String::from_utf8_lossy(&saved).into_owned();

    // No manual clean-up: `_master` reaps the process when this scope ends,
    // whether the assertions below pass or panic.
    assert!(
        load_elapsed < CLI_DEADLINE,
        "`state load` must answer within a bounded time, took {load_elapsed:?}"
    );
    assert!(
        save_ok,
        "`state save` must succeed.\nstdout=\n{}\nstderr=\n{}",
        String::from_utf8_lossy(&save.stdout),
        String::from_utf8_lossy(&save.stderr),
    );
    // The load-bearing assertion: the entry no worker acknowledged was reverted
    // from the main-process state, so `SaveState` cannot re-persist it and the
    // next replay cannot re-inject it.
    assert!(
        !saved.contains(REJECTED_HOSTNAME),
        "an entry every worker rejected must be reverted from the main-process state \
         and never re-persisted by SaveState (sozu#1313); saved state was:\n{saved}",
    );
    // …and the operator is told, instead of getting a silent success.
    assert!(
        !load.status.success(),
        "`state load` must report the worker rejection as a failure.\nstdout=\n{load_stdout}\nstderr=\n{load_stderr}",
    );
}
