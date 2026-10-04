//! A rejected main-upgrade candidate must leave the old command graph live.
//!
//! The master is started from a private copy of the real `sozu` binary. Once
//! client A has an in-flight worker upgrade behind an observable backend
//! barrier, that executable path is atomically replaced by a deliberately
//! incompatible candidate. The candidate records its PID and exits before it
//! can acknowledge the upgrade handoff.
//!
//! Rejection is pre-COMMIT: the old PID stays authoritative, A remains pending
//! and later completes once, and a fresh client C can still use the old Hub.
//! The rejected attempt must not publish a successful `main_upgraded` audit,
//! advance the boot generation, or leave the candidate child unreaped.
//!
//! This is a Linux-only, ignored process test. Run it with:
//!
//! ```bash
//! cargo test -p sozu --test main_upgrade_transfer_rejection_e2e --locked \
//!   -- --ignored --exact --nocapture --test-threads=1
//! ```
#![cfg(target_os = "linux")]

use std::{
    collections::BTreeSet,
    fs,
    io::{Read, Write},
    net::{TcpListener, TcpStream},
    os::unix::{fs::PermissionsExt, process::CommandExt},
    path::{Path, PathBuf},
    process::{Child, Command, Output, Stdio},
    sync::{Arc, Condvar, Mutex, mpsc},
    thread,
    time::{Duration, Instant},
};

const CONDITION_POLL: Duration = Duration::from_millis(25);

#[derive(Clone)]
struct BackendBarrier(Arc<(Mutex<bool>, Condvar)>);

impl BackendBarrier {
    fn new() -> Self {
        Self(Arc::new((Mutex::new(false), Condvar::new())))
    }

    fn wait(&self) {
        let (released, wake) = &*self.0;
        let mut released = released.lock().expect("lock backend barrier");
        while !*released {
            released = wake.wait(released).expect("wait on backend barrier");
        }
    }

    fn release(&self) {
        let (released, wake) = &*self.0;
        *released.lock().expect("lock backend barrier") = true;
        wake.notify_all();
    }
}

struct ProcessGuard {
    config_path: PathBuf,
    barrier: BackendBarrier,
    process_group: u32,
    original_main: Option<Child>,
}

fn finish_owned_process_group(process_group: u32, leader: &mut Child, grace_period: Duration) {
    let deadline = Instant::now() + grace_period;
    while Instant::now() < deadline
        && process_state(process_group).is_some_and(|state| state != 'Z')
    {
        thread::sleep(CONDITION_POLL);
    }

    // `leader` is deliberately not reaped until after this signal, so its PID
    // cannot be reused while it names the process group created by this test.
    // Kill that exact owned group even when the leader already exited: workers
    // or a failed upgrade candidate may still be live members of the PGID.
    let _ = nix::sys::signal::kill(
        nix::unistd::Pid::from_raw(-(process_group as i32)),
        nix::sys::signal::Signal::SIGKILL,
    );
    let _ = leader.wait();
}

impl Drop for ProcessGuard {
    fn drop(&mut self) {
        self.barrier.release();
        let _ = sozu(&self.config_path, &["shutdown"]);

        if let Some(main) = self.original_main.as_mut() {
            finish_owned_process_group(self.process_group, main, Duration::from_secs(10));
        }
    }
}

#[test]
fn owned_process_group_cleanup_reaches_descendant_after_leader_exit() {
    let temp = tempfile::tempdir().expect("test tempdir");
    let descendant_pid_file = temp.path().join("descendant.pid");
    let mut leader = Command::new("/bin/sh")
        .args([
            "-c",
            "sleep 60 & printf '%s\\n' \"$!\" > \"$1\"; exit 0",
            "owned-process-group",
            descendant_pid_file
                .to_str()
                .expect("UTF-8 descendant pid path"),
        ])
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .process_group(0)
        .spawn()
        .expect("spawn process-group leader");
    let process_group = leader.id();

    let setup_deadline = Instant::now() + Duration::from_secs(5);
    while (!descendant_pid_file.exists() || process_state(process_group) != Some('Z'))
        && Instant::now() < setup_deadline
    {
        thread::sleep(CONDITION_POLL);
    }
    assert_eq!(
        process_state(process_group),
        Some('Z'),
        "leader must be an unreaped zombie so its PID/PGID cannot be reused"
    );
    let descendant_pid: u32 = fs::read_to_string(&descendant_pid_file)
        .expect("leader must publish descendant PID")
        .trim()
        .parse()
        .expect("descendant PID must be numeric");
    assert!(
        process_state(descendant_pid).is_some_and(|state| state != 'Z'),
        "descendant must be live before cleanup"
    );

    finish_owned_process_group(process_group, &mut leader, Duration::ZERO);

    let cleanup_deadline = Instant::now() + Duration::from_secs(5);
    while process_state(descendant_pid).is_some_and(|state| state != 'Z')
        && Instant::now() < cleanup_deadline
    {
        thread::sleep(CONDITION_POLL);
    }
    let descendant_state_after_cleanup = process_state(descendant_pid);

    // Keep the deliberately red pre-fix run self-cleaning. This addresses the
    // exact group created above; a live member keeps that PGID from reuse.
    if descendant_state_after_cleanup.is_some_and(|state| state != 'Z') {
        let _ = nix::sys::signal::kill(
            nix::unistd::Pid::from_raw(-(process_group as i32)),
            nix::sys::signal::Signal::SIGKILL,
        );
    }

    assert!(
        descendant_state_after_cleanup.is_none_or(|state| state == 'Z'),
        "owned descendant survived cleanup after leader exit: state={descendant_state_after_cleanup:?}"
    );
}

fn free_port() -> u16 {
    TcpListener::bind("127.0.0.1:0")
        .expect("bind ephemeral port")
        .local_addr()
        .expect("ephemeral listener address")
        .port()
}

fn spawn_backend(received: mpsc::Sender<()>, barrier: BackendBarrier) -> u16 {
    let listener = TcpListener::bind("127.0.0.1:0").expect("bind backend");
    let port = listener.local_addr().expect("backend address").port();
    thread::spawn(move || {
        for stream in listener.incoming() {
            let Ok(mut stream) = stream else { continue };
            let received = received.clone();
            let barrier = barrier.clone();
            thread::spawn(move || {
                let mut request = Vec::new();
                let mut buffer = [0u8; 1024];
                while !request.windows(4).any(|window| window == b"\r\n\r\n") {
                    match stream.read(&mut buffer) {
                        Ok(0) | Err(_) => return,
                        Ok(read) => request.extend_from_slice(&buffer[..read]),
                    }
                }
                if request.starts_with(b"GET /held ") {
                    let _ = received.send(());
                    barrier.wait();
                }
                let _ = stream.write_all(
                    b"HTTP/1.1 200 OK\r\nContent-Length: 0\r\nConnection: close\r\n\r\n",
                );
            });
        }
    });
    port
}

fn get(port: u16, path: &str, timeout: Duration) -> Vec<u8> {
    let Ok(mut stream) = TcpStream::connect(("127.0.0.1", port)) else {
        return Vec::new();
    };
    let _ = stream.set_read_timeout(Some(timeout));
    let request =
        format!("GET {path} HTTP/1.1\r\nHost: rejection.test\r\nConnection: close\r\n\r\n");
    if stream.write_all(request.as_bytes()).is_err() {
        return Vec::new();
    }
    let mut response = Vec::new();
    let _ = stream.read_to_end(&mut response);
    response
}

fn sozu(config_path: &Path, args: &[&str]) -> Output {
    Command::new(env!("CARGO_BIN_EXE_sozu"))
        .args([
            "-c",
            config_path.to_str().expect("UTF-8 config path"),
            "-t",
            "30000",
        ])
        .args(args)
        .output()
        .expect("run sozu client")
}

fn running_worker_ids(config_path: &Path) -> Vec<u64> {
    let output = sozu(config_path, &["--json", "status"]);
    let Ok(status) = serde_json::from_slice::<serde_json::Value>(&output.stdout) else {
        return Vec::new();
    };
    status["WORKERS"]["vec"]
        .as_array()
        .into_iter()
        .flatten()
        .filter(|worker| worker["run_state"].as_i64() == Some(0))
        .filter_map(|worker| worker["id"].as_u64())
        .collect()
}

fn main_pid(pid_file: &Path) -> Option<u32> {
    fs::read_to_string(pid_file).ok()?.trim().parse().ok()
}

fn process_state(pid: u32) -> Option<char> {
    let stat = fs::read_to_string(format!("/proc/{pid}/stat")).ok()?;
    stat.rsplit_once(") ")?.1.chars().next()
}

fn children(pid: u32) -> BTreeSet<u32> {
    fs::read_to_string(format!("/proc/{pid}/task/{pid}/children"))
        .unwrap_or_default()
        .split_whitespace()
        .filter_map(|child| child.parse().ok())
        .collect()
}

fn audit_records(path: &Path) -> Vec<serde_json::Value> {
    fs::read_to_string(path)
        .unwrap_or_default()
        .lines()
        .map(|line| serde_json::from_str(line).expect("audit line must be valid JSON"))
        .collect()
}

fn state_saved_generation(path: &Path, state_path: &Path) -> u64 {
    let expected_target = format!("file:{}", state_path.display());
    audit_records(path)
        .into_iter()
        .rev()
        .find(|entry| {
            entry["verb"] == "state_saved"
                && entry["result"] == "ok"
                && entry["target"]
                    .as_str()
                    .is_some_and(|target| target.starts_with(&expected_target))
        })
        .and_then(|entry| entry["boot_generation"].as_u64())
        .expect("state save audit must expose its boot generation")
}

fn count_success(output: &Output, needle: &str) -> usize {
    String::from_utf8_lossy(&output.stdout)
        .matches(needle)
        .count()
        + String::from_utf8_lossy(&output.stderr)
            .matches(needle)
            .count()
}

#[test]
#[ignore = "process-level: spawns a real master/worker, replaces a private executable and binds ephemeral ports; run from the dedicated CI step or with --ignored"]
fn rejected_candidate_keeps_old_hub_authoritative_and_reaps_child() {
    let temp = tempfile::tempdir().expect("test tempdir");
    let socket_path = temp.path().join("sozu.sock");
    let config_path = temp.path().join("config.toml");
    let pid_file = temp.path().join("sozu.pid");
    let audit_path = temp.path().join("audit.jsonl");
    let installed_binary = temp.path().join("sozu-installed");
    let incompatible_candidate = temp.path().join("sozu-rejecting-candidate");
    let candidate_pid_file = temp.path().join("candidate.pid");
    let before_state = temp.path().join("before.state");
    let after_state = temp.path().join("after.state");

    fs::copy(env!("CARGO_BIN_EXE_sozu"), &installed_binary)
        .expect("copy real sozu binary to private install path");
    fs::set_permissions(&installed_binary, fs::Permissions::from_mode(0o755))
        .expect("make private sozu executable");

    let (held_received_tx, held_received) = mpsc::channel();
    let barrier = BackendBarrier::new();
    let backend_port = spawn_backend(held_received_tx, barrier.clone());
    let front_port = free_port();
    let config = format!(
        r#"
command_socket = "{socket}"
pid_file_path = "{pid_file}"
audit_logs_json_target = "{audit}"
command_buffer_size = 16384
max_command_buffer_size = 163840
worker_count = 1
worker_automatic_restart = false
handle_process_affinity = false
log_level = "info"
log_target = "stderr"
max_connections = 100
activate_listeners = true

[[listeners]]
protocol = "http"
address = "127.0.0.1:{front_port}"
back_timeout = 60

[clusters.rejection]
protocol = "http"
frontends = [ {{ address = "127.0.0.1:{front_port}", hostname = "rejection.test" }} ]
backends = [ {{ address = "127.0.0.1:{backend_port}", backend_id = "held-backend" }} ]
"#,
        socket = socket_path.display(),
        pid_file = pid_file.display(),
        audit = audit_path.display(),
    );
    fs::write(&config_path, config).expect("write config");

    let mut start = Command::new(&installed_binary);
    start
        .args([
            "start",
            "-c",
            config_path.to_str().expect("UTF-8 config path"),
        ])
        .env("SOZU_TEST_REJECT_PID_FILE", &candidate_pid_file)
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .process_group(0);
    let main = start.spawn().expect("spawn sozu from private install path");
    let original_main_pid = main.id();
    let mut guard = ProcessGuard {
        config_path: config_path.clone(),
        barrier: barrier.clone(),
        process_group: original_main_pid,
        original_main: Some(main),
    };

    let ready_deadline = Instant::now() + Duration::from_secs(20);
    while !get(front_port, "/ready", Duration::from_millis(250)).starts_with(b"HTTP/1.1 200") {
        assert!(Instant::now() < ready_deadline, "proxy never became ready");
        thread::sleep(CONDITION_POLL);
    }
    assert_eq!(main_pid(&pid_file), Some(original_main_pid));

    let before_save = sozu(
        &config_path,
        &[
            "state",
            "save",
            "-f",
            before_state.to_str().expect("UTF-8 state path"),
        ],
    );
    assert!(
        before_save.status.success(),
        "precondition state save failed: {}",
        String::from_utf8_lossy(&before_save.stderr)
    );
    let generation_before = state_saved_generation(&audit_path, &before_state);

    let held = thread::spawn(move || get(front_port, "/held", Duration::from_secs(40)));
    held_received
        .recv_timeout(Duration::from_secs(10))
        .expect("backend never observed held request");

    let mut client_a = Command::new(env!("CARGO_BIN_EXE_sozu"))
        .args([
            "-c",
            config_path.to_str().expect("UTF-8 config path"),
            "-t",
            "30000",
        ])
        .args(["upgrade", "--worker", "0"])
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .expect("spawn client A worker upgrade");
    let replacement_deadline = Instant::now() + Duration::from_secs(20);
    while !running_worker_ids(&config_path).contains(&1) {
        assert!(
            Instant::now() < replacement_deadline,
            "worker 0 was never replaced by worker 1"
        );
        thread::sleep(CONDITION_POLL);
    }
    assert!(
        client_a.try_wait().expect("observe client A").is_none(),
        "client A completed before the main-upgrade rejection scenario"
    );
    let children_before = children(original_main_pid);

    fs::write(
        &incompatible_candidate,
        b"#!/bin/sh\nprintf '%s\\n' \"$$\" > \"$SOZU_TEST_REJECT_PID_FILE\"\nexit 78\n",
    )
    .expect("write incompatible candidate");
    fs::set_permissions(&incompatible_candidate, fs::Permissions::from_mode(0o755))
        .expect("make incompatible candidate executable");
    fs::rename(&incompatible_candidate, &installed_binary)
        .expect("atomically install incompatible candidate");

    let client_b = sozu(&config_path, &["upgrade"]);
    let candidate_pid: u32 = fs::read_to_string(&candidate_pid_file)
        .expect("candidate must record that exec reached it")
        .trim()
        .parse()
        .expect("candidate PID must be numeric");

    let old_main_still_authoritative = main_pid(&pid_file) == Some(original_main_pid)
        && process_state(original_main_pid).is_some_and(|state| state != 'Z');
    let client_a_pending_after_rejection = client_a
        .try_wait()
        .expect("observe client A after rejected upgrade")
        .is_none();
    let children_after_rejection = children(original_main_pid);
    let candidate_state = process_state(candidate_pid);

    barrier.release();
    let held_response = held.join().expect("held request thread");
    let client_a_deadline = Instant::now() + Duration::from_secs(10);
    while client_a
        .try_wait()
        .expect("observe client A terminal")
        .is_none()
        && Instant::now() < client_a_deadline
    {
        thread::sleep(CONDITION_POLL);
    }
    let client_a_timed_out = client_a
        .try_wait()
        .expect("observe client A at deadline")
        .is_none();
    if client_a_timed_out {
        let _ = client_a.kill();
    }
    let client_a_output = client_a
        .wait_with_output()
        .expect("collect client A output");

    let client_c = sozu(
        &config_path,
        &[
            "state",
            "save",
            "-f",
            after_state.to_str().expect("UTF-8 state path"),
        ],
    );
    let generation_after = client_c
        .status
        .success()
        .then(|| state_saved_generation(&audit_path, &after_state));
    let audits = audit_records(&audit_path);
    let false_upgrade_successes: Vec<_> = audits
        .iter()
        .filter(|entry| entry["verb"] == "main_upgraded" && entry["result"] == "ok")
        .collect();

    let _ = sozu(&config_path, &["shutdown"]);
    if let Some(main) = guard.original_main.as_mut() {
        let deadline = Instant::now() + Duration::from_secs(10);
        while main
            .try_wait()
            .expect("observe old main shutdown")
            .is_none()
            && Instant::now() < deadline
        {
            thread::sleep(CONDITION_POLL);
        }
    }

    let mut violations = Vec::new();
    if client_b.status.success() {
        violations.push(format!(
            "client B reported success for incompatible candidate: stdout={:?} stderr={:?}",
            String::from_utf8_lossy(&client_b.stdout),
            String::from_utf8_lossy(&client_b.stderr)
        ));
    }
    let client_b_successes = count_success(&client_b, "Upgrade successful");
    if client_b_successes != 0 {
        violations.push(format!(
            "client B published {client_b_successes} false terminal successes"
        ));
    }
    if !old_main_still_authoritative {
        violations.push(
            "rejected pre-COMMIT transfer changed or killed the authoritative main".to_owned(),
        );
    }
    if !client_a_pending_after_rejection {
        violations
            .push("client A terminated while the incompatible candidate was rejected".to_owned());
    }
    if !held_response.starts_with(b"HTTP/1.1 200") {
        violations.push(format!(
            "held request did not complete after rejection: {:?}",
            String::from_utf8_lossy(&held_response)
        ));
    }
    if client_a_timed_out || !client_a_output.status.success() {
        violations.push(format!(
            "client A did not complete successfully: status={:?} stdout={:?} stderr={:?}",
            client_a_output.status,
            String::from_utf8_lossy(&client_a_output.stdout),
            String::from_utf8_lossy(&client_a_output.stderr)
        ));
    }
    let client_a_successes = count_success(&client_a_output, "Upgrade successful");
    if client_a_successes != 1 {
        violations.push(format!(
            "client A received {client_a_successes} terminal successes instead of exactly one"
        ));
    }
    if !client_c.status.success() {
        violations.push(format!(
            "client C could not use the old Hub after rejection: {}",
            String::from_utf8_lossy(&client_c.stderr)
        ));
    }
    if generation_after != Some(generation_before) {
        violations.push(format!(
            "rejected pre-COMMIT transfer advanced boot generation: before={generation_before}, after={generation_after:?}"
        ));
    }
    if !false_upgrade_successes.is_empty() {
        violations.push(format!(
            "rejected pre-COMMIT transfer published successful main-upgrade audits: {false_upgrade_successes:?}"
        ));
    }
    if candidate_state.is_some() {
        violations.push(format!(
            "candidate child {candidate_pid} was not reaped (state={candidate_state:?})"
        ));
    }
    if children_after_rejection != children_before {
        violations.push(format!(
            "rejected candidate changed the old main's child set: before={children_before:?} after={children_after_rejection:?}"
        ));
    }

    assert!(
        violations.is_empty(),
        "pre-COMMIT rejection contract violations:\n{}",
        violations.join("\n")
    );
}

#[test]
#[ignore = "process-level: holds a replacement main in PREPARE, signals the old main and observes abort/reap before COMMIT"]
fn sigterm_during_prepare_aborts_upgrade_then_stops_the_old_main() {
    let temp = tempfile::tempdir().expect("test tempdir");
    let socket_path = temp.path().join("sozu.sock");
    let config_path = temp.path().join("config.toml");
    let pid_file = temp.path().join("sozu.pid");
    let audit_path = temp.path().join("audit.jsonl");
    let installed_binary = temp.path().join("sozu-installed");
    let real_candidate = temp.path().join("sozu-option3");
    let waiting_candidate = temp.path().join("sozu-waiting-candidate");
    let candidate_pid_file = temp.path().join("candidate.pid");
    let release_file = temp.path().join("release-candidate");

    fs::copy(env!("CARGO_BIN_EXE_sozu"), &installed_binary)
        .expect("copy real sozu binary to private install path");
    fs::copy(env!("CARGO_BIN_EXE_sozu"), &real_candidate)
        .expect("copy real candidate away from the path replaced by the wrapper");
    for path in [&installed_binary, &real_candidate] {
        fs::set_permissions(path, fs::Permissions::from_mode(0o755))
            .expect("make private sozu executable");
    }

    let config = format!(
        r#"
command_socket = "{socket}"
pid_file_path = "{pid_file}"
audit_logs_json_target = "{audit}"
command_buffer_size = 16384
max_command_buffer_size = 163840
worker_count = 1
worker_automatic_restart = false
handle_process_affinity = false
log_level = "info"
log_target = "stderr"
activate_listeners = false
"#,
        socket = socket_path.display(),
        pid_file = pid_file.display(),
        audit = audit_path.display(),
    );
    fs::write(&config_path, config).expect("write config");

    let mut start = Command::new(&installed_binary);
    start
        .args([
            "start",
            "-c",
            config_path.to_str().expect("UTF-8 config path"),
        ])
        .env("SOZU_TEST_REAL_CANDIDATE", &real_candidate)
        .env("SOZU_TEST_CANDIDATE_PID", &candidate_pid_file)
        .env("SOZU_TEST_CANDIDATE_RELEASE", &release_file)
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .process_group(0);
    let main = start.spawn().expect("spawn sozu from private install path");
    let original_main_pid = main.id();
    let barrier = BackendBarrier::new();
    let mut guard = ProcessGuard {
        config_path: config_path.clone(),
        barrier,
        process_group: original_main_pid,
        original_main: Some(main),
    };

    let ready_deadline = Instant::now() + Duration::from_secs(20);
    while !sozu(&config_path, &["--json", "status"]).status.success() {
        assert!(Instant::now() < ready_deadline, "main never became ready");
        thread::sleep(CONDITION_POLL);
    }
    assert_eq!(main_pid(&pid_file), Some(original_main_pid));

    fs::write(
        &waiting_candidate,
        br#"#!/bin/sh
if [ "$1" = "upgrade-probe" ]; then
  exit 0
fi
candidate_pid_staging="${SOZU_TEST_CANDIDATE_PID}.tmp.$$"
printf '%s\n' "$$" > "$candidate_pid_staging" || exit 1
mv "$candidate_pid_staging" "$SOZU_TEST_CANDIDATE_PID" || exit 1
while [ ! -e "$SOZU_TEST_CANDIDATE_RELEASE" ]; do
  sleep 0.01
done
exec "$SOZU_TEST_REAL_CANDIDATE" "$@"
"#,
    )
    .expect("write waiting candidate wrapper");
    fs::set_permissions(&waiting_candidate, fs::Permissions::from_mode(0o755))
        .expect("make waiting candidate executable");
    fs::rename(&waiting_candidate, &installed_binary)
        .expect("atomically install waiting candidate");

    let mut client_b = Command::new(env!("CARGO_BIN_EXE_sozu"))
        .args([
            "-c",
            config_path.to_str().expect("UTF-8 config path"),
            "-t",
            "30000",
            "upgrade",
        ])
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .expect("spawn main-upgrade client");

    let prepare_deadline = Instant::now() + Duration::from_secs(10);
    while !candidate_pid_file.exists() && Instant::now() < prepare_deadline {
        thread::sleep(CONDITION_POLL);
    }
    let candidate_pid: u32 = fs::read_to_string(&candidate_pid_file)
        .expect("candidate wrapper must report reaching PREPARE")
        .trim()
        .parse()
        .expect("candidate PID must be numeric");
    assert!(
        client_b.try_wait().expect("observe client B").is_none(),
        "upgrade completed before the PREPARE barrier was released"
    );

    nix::sys::signal::kill(
        nix::unistd::Pid::from_raw(original_main_pid as i32),
        nix::sys::signal::Signal::SIGTERM,
    )
    .expect("signal the old main while the child is in PREPARE");
    fs::write(&release_file, b"release\n").expect("release candidate PREPARE barrier");

    let client_deadline = Instant::now() + Duration::from_secs(20);
    while client_b
        .try_wait()
        .expect("observe main-upgrade client")
        .is_none()
        && Instant::now() < client_deadline
    {
        thread::sleep(CONDITION_POLL);
    }
    assert!(
        client_b
            .try_wait()
            .expect("observe main-upgrade client deadline")
            .is_some(),
        "main-upgrade client did not receive the pre-COMMIT rejection"
    );
    let client_b = client_b
        .wait_with_output()
        .expect("collect client B output");

    let stop_deadline = Instant::now() + Duration::from_secs(20);
    let old_main_status = loop {
        if let Some(status) = guard
            .original_main
            .as_mut()
            .expect("old main guard")
            .try_wait()
            .expect("observe old main")
        {
            break status;
        }
        assert!(
            Instant::now() < stop_deadline,
            "old main did not drain the pending SIGTERM after aborting PREPARE"
        );
        thread::sleep(CONDITION_POLL);
    };

    let candidate_deadline = Instant::now() + Duration::from_secs(5);
    while process_state(candidate_pid).is_some() && Instant::now() < candidate_deadline {
        thread::sleep(CONDITION_POLL);
    }
    let successful_upgrade_audits = audit_records(&audit_path)
        .into_iter()
        .filter(|entry| entry["verb"] == "main_upgraded" && entry["result"] == "ok")
        .count();

    assert!(old_main_status.success(), "old main stopped unsuccessfully");
    assert!(
        !client_b.status.success(),
        "client B reported success even though SIGTERM won before COMMIT: stdout={:?} stderr={:?}",
        String::from_utf8_lossy(&client_b.stdout),
        String::from_utf8_lossy(&client_b.stderr)
    );
    assert_eq!(count_success(&client_b, "Upgrade successful"), 0);
    assert_eq!(
        process_state(candidate_pid),
        None,
        "aborted candidate {candidate_pid} was not reaped"
    );
    assert_eq!(
        successful_upgrade_audits, 0,
        "pre-COMMIT signal abort published a successful main-upgrade audit"
    );
}
