//! Main-upgrade protocol compatibility must fail before candidate adoption.
//!
//! Each case starts the sender from a private copy of one immutable binary.
//! Once client A has an in-flight worker upgrade behind an observable backend
//! barrier, that executable path is atomically replaced by the other immutable
//! binary. The real candidate must reject the incompatible handoff before it
//! adopts the Hub.
//!
//! Both directions require the sender PID to stay authoritative, A to remain
//! pending and later complete once, a fresh client C to use the sender Hub, and
//! the candidate child to be reaped. The Option3 sender additionally promises
//! transactional rejection: no successful `main_upgraded` audit and no boot
//! generation change. The legacy sender predates that transaction contract, so
//! its audit and generation observations are reported without certifying them.
//!
//! This is a Linux-only, ignored process test. Run it with:
//!
//! ```bash
//! SOZU_MATRIX_LEGACY=/path/to/legacy SOZU_MATRIX_OPTION3=/path/to/option3 \
//! cargo test -p sozu --test main_upgrade_compatibility_matrix_e2e --locked \
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
    client_binary: PathBuf,
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
        let _ = sozu(&self.client_binary, &self.config_path, &["shutdown"]);

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

fn sozu(binary: &Path, config_path: &Path, args: &[&str]) -> Output {
    Command::new(binary)
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

fn running_worker_ids(binary: &Path, config_path: &Path) -> Vec<u64> {
    let output = sozu(binary, config_path, &["--json", "status"]);
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

#[derive(Clone, Copy)]
enum Direction {
    Option3ToLegacy,
    LegacyToOption3,
}

impl Direction {
    fn label(self) -> &'static str {
        match self {
            Self::Option3ToLegacy => "option3-to-legacy",
            Self::LegacyToOption3 => "legacy-to-option3",
        }
    }

    /// The frozen `(sender, candidate)` binaries, or `None` when either
    /// variable is unset or empty.
    fn binaries(self) -> Option<(PathBuf, PathBuf)> {
        let legacy = std::env::var_os("SOZU_MATRIX_LEGACY")
            .filter(|path| !path.is_empty())
            .map(PathBuf::from)?;
        let option3 = std::env::var_os("SOZU_MATRIX_OPTION3")
            .filter(|path| !path.is_empty())
            .map(PathBuf::from)?;
        Some(match self {
            Self::Option3ToLegacy => (option3, legacy),
            Self::LegacyToOption3 => (legacy, option3),
        })
    }

    fn transactional_sender(self) -> bool {
        matches!(self, Self::Option3ToLegacy)
    }
}

fn wait_for_children(parent: u32, expected: &BTreeSet<u32>, timeout: Duration) -> BTreeSet<u32> {
    let deadline = Instant::now() + timeout;
    loop {
        let observed = children(parent);
        if &observed == expected || Instant::now() >= deadline {
            return observed;
        }
        thread::sleep(CONDITION_POLL);
    }
}

fn run_compatibility_case(direction: Direction) {
    // Both binaries are built out of tree, so a plain `-- --ignored` run (and
    // CI, which never sets the variables) skips this case instead of failing
    // the whole test binary. libtest has no skipped outcome, so the case still
    // reports `ok`; writing to stderr directly instead of through
    // `println!`/`eprintln!` bypasses libtest's output capture and keeps the
    // skip visible without `--nocapture`.
    let Some((sender_binary, candidate_binary)) = direction.binaries() else {
        let _ = writeln!(
            std::io::stderr(),
            "skipping {}: set SOZU_MATRIX_LEGACY and SOZU_MATRIX_OPTION3 to the frozen binaries (see module docs)",
            direction.label()
        );
        return;
    };
    assert!(sender_binary.is_file(), "sender binary is missing");
    assert!(candidate_binary.is_file(), "candidate binary is missing");

    let temp = tempfile::tempdir().expect("test tempdir");
    let socket_path = temp.path().join("sozu.sock");
    let config_path = temp.path().join("config.toml");
    let pid_file = temp.path().join("sozu.pid");
    let audit_path = temp.path().join("audit.jsonl");
    let installed_binary = temp.path().join("sozu-installed");
    let staged_candidate = temp.path().join("sozu-candidate");
    let before_state = temp.path().join("before.state");
    let after_state = temp.path().join("after.state");

    fs::copy(&sender_binary, &installed_binary).expect("copy sender to private install path");
    fs::set_permissions(&installed_binary, fs::Permissions::from_mode(0o755))
        .expect("make private sender executable");

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

[clusters.compatibility]
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
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .process_group(0);
    let main = start
        .spawn()
        .expect("spawn sender from private install path");
    let original_main_pid = main.id();
    let mut guard = ProcessGuard {
        config_path: config_path.clone(),
        client_binary: sender_binary.clone(),
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
        &sender_binary,
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

    let mut client_a = Command::new(&sender_binary)
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
    while !running_worker_ids(&sender_binary, &config_path).contains(&1) {
        assert!(
            Instant::now() < replacement_deadline,
            "worker 0 was never replaced by worker 1"
        );
        thread::sleep(CONDITION_POLL);
    }
    assert!(
        client_a.try_wait().expect("observe client A").is_none(),
        "client A completed before the compatibility probe"
    );
    let children_before = children(original_main_pid);

    fs::copy(&candidate_binary, &staged_candidate).expect("stage real candidate binary");
    fs::set_permissions(&staged_candidate, fs::Permissions::from_mode(0o755))
        .expect("make real candidate executable");
    fs::rename(&staged_candidate, &installed_binary).expect("atomically install real candidate");

    let client_b = sozu(&sender_binary, &config_path, &["upgrade"]);
    let old_main_still_authoritative = main_pid(&pid_file) == Some(original_main_pid)
        && process_state(original_main_pid).is_some_and(|state| state != 'Z');
    let client_a_pending_after_rejection = client_a
        .try_wait()
        .expect("observe client A after compatibility rejection")
        .is_none();
    let children_after_rejection =
        wait_for_children(original_main_pid, &children_before, Duration::from_secs(5));
    let candidate_children: Vec<_> = children_after_rejection
        .difference(&children_before)
        .copied()
        .collect();
    let candidate_states_before_cleanup: Vec<_> = candidate_children
        .iter()
        .map(|pid| (*pid, process_state(*pid)))
        .collect();

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
        &sender_binary,
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

    let _ = sozu(&sender_binary, &config_path, &["shutdown"]);
    if let Some(main) = guard.original_main.as_mut() {
        let deadline = Instant::now() + Duration::from_secs(10);
        while main.try_wait().expect("observe sender shutdown").is_none()
            && Instant::now() < deadline
        {
            thread::sleep(CONDITION_POLL);
        }
    }
    if let Some(mut main) = guard.original_main.take() {
        finish_owned_process_group(guard.process_group, &mut main, Duration::from_secs(10));
    }
    let cleanup_deadline = Instant::now() + Duration::from_secs(5);
    while candidate_children
        .iter()
        .any(|pid| process_state(*pid).is_some())
        && Instant::now() < cleanup_deadline
    {
        thread::sleep(CONDITION_POLL);
    }
    let candidate_states_after_cleanup: Vec<_> = candidate_children
        .iter()
        .map(|pid| (*pid, process_state(*pid)))
        .collect();

    eprintln!(
        "COMPAT direction={} sender_pid={} client_b_status={:?} generation_before={} generation_after={:?} main_upgrade_success_audits={} children_before={:?} children_after={:?} candidate_states_before_cleanup={:?} candidate_states_after_cleanup={:?}",
        direction.label(),
        original_main_pid,
        client_b.status.code(),
        generation_before,
        generation_after,
        false_upgrade_successes.len(),
        children_before,
        children_after_rejection,
        candidate_states_before_cleanup,
        candidate_states_after_cleanup,
    );
    eprintln!(
        "COMPAT direction={} client_b_stdout={:?} client_b_stderr={:?}",
        direction.label(),
        String::from_utf8_lossy(&client_b.stdout),
        String::from_utf8_lossy(&client_b.stderr),
    );

    let mut violations = Vec::new();
    if client_b.status.success() {
        violations.push(format!(
            "{} client B reported success for incompatible candidate",
            direction.label()
        ));
    }
    let client_b_successes = count_success(&client_b, "Upgrade successful");
    if client_b_successes != 0 {
        violations.push(format!(
            "{} client B published {client_b_successes} false terminal successes",
            direction.label()
        ));
    }
    if !old_main_still_authoritative {
        violations.push(format!(
            "{} changed or killed the authoritative sender main",
            direction.label()
        ));
    }
    if !client_a_pending_after_rejection {
        violations.push(format!(
            "{} terminated client A during compatibility rejection",
            direction.label()
        ));
    }
    if !held_response.starts_with(b"HTTP/1.1 200") {
        violations.push(format!(
            "{} held request did not complete: {:?}",
            direction.label(),
            String::from_utf8_lossy(&held_response)
        ));
    }
    if client_a_timed_out || !client_a_output.status.success() {
        violations.push(format!(
            "{} client A did not complete successfully: status={:?} stdout={:?} stderr={:?}",
            direction.label(),
            client_a_output.status,
            String::from_utf8_lossy(&client_a_output.stdout),
            String::from_utf8_lossy(&client_a_output.stderr)
        ));
    }
    let client_a_successes = count_success(&client_a_output, "Upgrade successful");
    if client_a_successes != 1 {
        violations.push(format!(
            "{} client A received {client_a_successes} terminal successes instead of exactly one",
            direction.label()
        ));
    }
    if !client_c.status.success() {
        violations.push(format!(
            "{} client C could not use the sender Hub: {}",
            direction.label(),
            String::from_utf8_lossy(&client_c.stderr)
        ));
    }
    if candidate_states_after_cleanup
        .iter()
        .any(|(_, state)| state.is_some())
    {
        violations.push(format!(
            "{} left candidate processes after owned PGID cleanup: {candidate_states_after_cleanup:?}",
            direction.label(),
        ));
    }
    if direction.transactional_sender() {
        if children_after_rejection != children_before {
            violations.push(format!(
                "Option3 did not reap the rejected candidate before returning: children before={children_before:?}, after={children_after_rejection:?}"
            ));
        }
        if generation_after != Some(generation_before) {
            violations.push(format!(
                "Option3 rejection advanced boot generation: before={generation_before}, after={generation_after:?}"
            ));
        }
        if !false_upgrade_successes.is_empty() {
            violations.push(format!(
                "Option3 rejection published successful main-upgrade audits: {false_upgrade_successes:?}"
            ));
        }
    }

    assert!(
        violations.is_empty(),
        "compatibility contract violations:\n{}",
        violations.join("\n")
    );
}

#[test]
#[ignore = "process-level compatibility matrix; requires SOZU_MATRIX_LEGACY and SOZU_MATRIX_OPTION3"]
fn option3_sender_rejects_legacy_candidate_before_adoption() {
    run_compatibility_case(Direction::Option3ToLegacy);
}

#[test]
#[ignore = "process-level compatibility matrix; requires SOZU_MATRIX_LEGACY and SOZU_MATRIX_OPTION3"]
fn legacy_sender_survives_option3_candidate_refusal() {
    run_compatibility_case(Direction::LegacyToOption3);
}
