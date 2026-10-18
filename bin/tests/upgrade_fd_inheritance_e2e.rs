//! sozu#1668 end-to-end test: after worker and main upgrades, no sozu process
//! may hold a descriptor that belongs to another one.
//!
//! Every exec'd process inherits each descriptor its parent has not marked
//! close-on-exec. The main process hands over, on purpose, only the command
//! socket, each adopted worker's channel and SCM socket (to the next main) and
//! a worker's own channel, SCM socket and state file (to that worker). Before
//! the fix it also leaked: the main end of every worker's SCM socket into every
//! later worker, the channels of stopped workers and the upgrade-state file
//! and confirmation channel into the next main, and from there into its
//! workers, while each worker kept its deleted state file open. A worker that
//! holds a copy of another worker's main-side socket keeps that socket alive,
//! so its peer never sees EOF when the other end goes away.
//!
//! Spawns the real `sozu` binary (`CARGO_BIN_EXE_sozu`) with two workers, a
//! TCP and an HTTP listener, then runs `upgrade --worker 0`, `upgrade --worker
//! 1` and a main `upgrade`. After each step it reads `/proc/<pid>/fd` of the
//! main process (from the pid file) and of every running worker (from `sozu
//! --json status`) and fails when a socket inode is held by two sozu processes,
//! twice by one process, or when a process holds a deleted file. Linux-only.
//!
//! The main is hardened (non-dumpable), so its `/proc/<pid>/fd` is owned by
//! root of its user namespace. The fixture spawns it directly: this test
//! executable must itself run as root of a user namespace (`unshare --user
//! --map-root-user`) to list the main's descriptors. Run directly as an
//! ordinary user, the test fails naming the process, pid and path instead of
//! skipping it.
//!
//! `#[ignore]`d so a contributor's `cargo test` stays fast; CI runs the compiled
//! test executable under `sudo setpriv … unshare --user --map-root-user`, which
//! maps the normal runner owner UID/GID to namespace root, in the
//! `proc-observer-e2e` job of
//! `.github/workflows/ci.yml`. Run it by hand as root of a user namespace, for
//! example:
//!
//! ```bash
//! cargo test -p sozu --test upgrade_fd_inheritance_e2e --no-run
//! unshare --user --map-root-user -- target/debug/deps/upgrade_fd_inheritance_e2e-<hash> \
//!   --ignored --exact upgrades_leave_no_descriptor_shared_between_sozu_processes \
//!   --nocapture --test-threads=1
//! ```
#![cfg(target_os = "linux")]

use std::{
    collections::BTreeMap,
    net::TcpListener,
    os::unix::process::CommandExt,
    path::Path,
    process::{Child, Command, Stdio},
    thread,
    time::{Duration, Instant},
};

/// Owns the private process group of the spawned main, so a panic anywhere in
/// the test still reaches the main, its workers and any upgrade candidate.
struct ProcessGuard {
    process_group: u32,
    leader: Child,
}

impl ProcessGuard {
    fn new(leader: Child) -> Self {
        Self {
            process_group: leader.id(),
            leader,
        }
    }
}

impl Drop for ProcessGuard {
    fn drop(&mut self) {
        // The leader is reaped only after this signal, so its PID cannot be
        // reused while it names the process group this test created. The
        // group is killed even when the leader already exited: workers or the
        // upgraded main may still be members of it.
        let _ = nix::sys::signal::kill(
            nix::unistd::Pid::from_raw(-(self.process_group as i32)),
            nix::sys::signal::Signal::SIGKILL,
        );
        let _ = self.leader.wait();
    }
}

fn free_port() -> u16 {
    TcpListener::bind("127.0.0.1:0")
        .expect("bind ephemeral port")
        .local_addr()
        .expect("local_addr")
        .port()
}

fn sozu(config_path: &Path, args: &[&str]) -> std::process::Output {
    Command::new(env!("CARGO_BIN_EXE_sozu"))
        .args(["-c", config_path.to_str().unwrap(), "-t", "30000"])
        .args(args)
        .output()
        .expect("run sozu")
}

/// Pids of the workers the master reports as running, from `sozu --json
/// status`. Empty when the master does not answer yet.
fn running_workers(config_path: &Path) -> Vec<u32> {
    let output = sozu(config_path, &["--json", "status"]);
    let Ok(status) = serde_json::from_slice::<serde_json::Value>(&output.stdout) else {
        return Vec::new();
    };
    status["WORKERS"]["vec"]
        .as_array()
        .into_iter()
        .flatten()
        // `RunState::Running` is 0 on the wire
        .filter(|worker| worker["run_state"].as_i64() == Some(0))
        .filter_map(|worker| worker["pid"].as_u64())
        .filter_map(|pid| u32::try_from(pid).ok())
        .collect()
}

fn is_gone(pid: u32) -> bool {
    std::fs::read_to_string(format!("/proc/{pid}/stat")).map_or(true, |stat| stat.contains(") Z "))
}

fn main_pid(pid_file: &Path) -> Option<u32> {
    std::fs::read_to_string(pid_file).ok()?.trim().parse().ok()
}

/// Wait until the master answers with two running workers, the set differs
/// from `before` (unless `before` is empty), and every pid of `before` that is
/// no longer running has exited.
fn wait_for_workers(config_path: &Path, before: &[u32]) -> Vec<u32> {
    let deadline = Instant::now() + Duration::from_secs(30);
    loop {
        let workers = running_workers(config_path);
        if workers.len() == 2
            && (before.is_empty() || workers.as_slice() != before)
            && before
                .iter()
                .filter(|pid| !workers.contains(pid))
                .all(|pid| is_gone(*pid))
        {
            return workers;
        }
        assert!(
            Instant::now() < deadline,
            "expected two settled running workers, got {workers:?} (before {before:?})"
        );
        thread::sleep(Duration::from_millis(100));
    }
}

/// Every socket inode and deleted file each process holds beyond its standard
/// streams, as `(process label, fd, target)`. Every process listed here was
/// settled before this call, so a descriptor that cannot be observed (the
/// directory, an entry or a link) fails the test with its context instead of
/// being skipped: an unobserved process proves nothing about leaks.
fn descriptors(step: &str, processes: &[(String, u32)]) -> Vec<(String, String, String)> {
    let mut found = Vec::new();
    for (label, pid) in processes {
        let fd_dir = format!("/proc/{pid}/fd");
        let entries = std::fs::read_dir(&fd_dir).unwrap_or_else(|error| {
            panic!("{step}: {label} (pid {pid}): read_dir {fd_dir}: {error}")
        });
        for entry in entries {
            let entry = entry.unwrap_or_else(|error| {
                panic!("{step}: {label} (pid {pid}): read entry of {fd_dir}: {error}")
            });
            let fd = entry.file_name().to_string_lossy().into_owned();
            // Standard input, output and error are inherited by design.
            if matches!(fd.as_str(), "0" | "1" | "2") {
                continue;
            }
            let target = std::fs::read_link(entry.path()).unwrap_or_else(|error| {
                panic!(
                    "{step}: {label} (pid {pid}): read_link {} (fd {fd}): {error}",
                    entry.path().display()
                )
            });
            let target = target.to_string_lossy().into_owned();
            if target.starts_with("socket:[") || target.ends_with(" (deleted)") {
                found.push((label.to_owned(), fd, target));
            }
        }
    }
    found
}

/// The violations after `step`: a socket inode held twice, anywhere, and any
/// deleted file still open.
fn violations(step: &str, processes: &[(String, u32)]) -> Vec<String> {
    let found = descriptors(step, processes);
    let mut by_socket: BTreeMap<&str, Vec<String>> = BTreeMap::new();
    let mut problems = Vec::new();
    for (label, fd, target) in &found {
        if target.starts_with("socket:[") {
            by_socket
                .entry(target.as_str())
                .or_default()
                .push(format!("{label} fd {fd}"));
        } else {
            problems.push(format!("{step}: {label} fd {fd} holds {target}"));
        }
    }
    for (socket, holders) in by_socket {
        if holders.len() > 1 {
            problems.push(format!("{step}: {socket} held by {}", holders.join(", ")));
        }
    }
    problems
}

fn processes(pid_file: &Path, workers: &[u32]) -> Vec<(String, u32)> {
    let mut processes = vec![("main".to_owned(), main_pid(pid_file).expect("pid file"))];
    processes.extend(
        workers
            .iter()
            .map(|pid| (format!("worker pid {pid}"), *pid)),
    );
    processes
}

/// Ask the current main to shut down and give it time to go. The
/// [`ProcessGuard`] is the backstop, and the original leader is never reaped
/// here.
fn stop(config_path: &Path, pid_file: &Path) {
    let main = main_pid(pid_file);
    let _ = sozu(config_path, &["shutdown"]);
    let deadline = Instant::now() + Duration::from_secs(20);
    while Instant::now() < deadline && main.is_some_and(|pid| !is_gone(pid)) {
        thread::sleep(Duration::from_millis(50));
    }
}

#[test]
#[ignore = "process-level: spawns a real master and workers, upgrades them and binds ephemeral ports; run from the dedicated CI step or with --ignored (see module docs)"]
fn upgrades_leave_no_descriptor_shared_between_sozu_processes() {
    let temp = tempfile::tempdir().expect("tempdir");
    let socket_path = temp.path().join("sozu.sock");
    let config_path = temp.path().join("config.toml");
    let pid_file = temp.path().join("sozu.pid");

    let config = format!(
        r#"
command_socket = "{socket}"
pid_file_path = "{pid_file}"
command_buffer_size = 16384
max_command_buffer_size = 163840
worker_count = 2
worker_automatic_restart = false
handle_process_affinity = false
log_level = "info"
log_target = "stderr"
max_connections = 100
activate_listeners = true

[[listeners]]
protocol = "tcp"
address = "127.0.0.1:{tcp_port}"

[[listeners]]
protocol = "http"
address = "127.0.0.1:{http_port}"
"#,
        socket = socket_path.display(),
        pid_file = pid_file.display(),
        tcp_port = free_port(),
        http_port = free_port(),
    );
    std::fs::write(&config_path, &config).expect("write config");

    let guard = ProcessGuard::new(
        Command::new(env!("CARGO_BIN_EXE_sozu"))
            .args(["start", "-c", config_path.to_str().unwrap()])
            .process_group(0)
            .spawn()
            .expect("spawn sozu start"),
    );

    let mut problems = Vec::new();
    let workers = wait_for_workers(&config_path, &[]);
    problems.extend(violations("start", &processes(&pid_file, &workers)));

    let mut current = workers;
    for index in 0..2 {
        let worker_id = index.to_string();
        let output = sozu(&config_path, &["upgrade", "--worker", &worker_id]);
        assert!(
            output.status.success(),
            "upgrade --worker {worker_id} failed: {}",
            String::from_utf8_lossy(&output.stderr)
        );
        current = wait_for_workers(&config_path, &current);
        problems.extend(violations(
            &format!("after upgrade --worker {worker_id}"),
            &processes(&pid_file, &current),
        ));
    }

    let old_main = main_pid(&pid_file).expect("pid file");
    let output = sozu(&config_path, &["upgrade"]);
    assert!(
        output.status.success(),
        "main upgrade failed: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    let deadline = Instant::now() + Duration::from_secs(30);
    while main_pid(&pid_file) == Some(old_main) || !is_gone(old_main) {
        assert!(Instant::now() < deadline, "the old main never handed over");
        thread::sleep(Duration::from_millis(100));
    }
    let current = wait_for_workers(&config_path, &current);
    problems.extend(violations(
        "after the main upgrade",
        &processes(&pid_file, &current),
    ));

    stop(&config_path, &pid_file);
    drop(guard);

    assert!(
        problems.is_empty(),
        "descriptors leaked between sozu processes:\n{}",
        problems.join("\n")
    );
}

#[test]
fn process_guard_kills_owned_group_descendant_when_the_test_panics() {
    let temp = tempfile::tempdir().expect("tempdir");
    let descendant_pid_file = temp.path().join("descendant.pid");
    // The guard owns the group from the instant the leader exists, so a panic
    // in any later setup step still unwinds through it.
    let guard = ProcessGuard::new(
        Command::new("/bin/sh")
            .args([
                "-c",
                "sleep 60 & printf '%s\\n' \"$!\" > \"$1\"; exit 0",
                "owned-process-group",
                descendant_pid_file.to_str().expect("UTF-8 path"),
            ])
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .process_group(0)
            .spawn()
            .expect("spawn process-group leader"),
    );
    let process_group = guard.process_group;

    // The leader exits after publishing the descendant; it stays an unreaped
    // zombie, so its PID cannot be reused while it names the group.
    let deadline = Instant::now() + Duration::from_secs(5);
    while !is_gone(process_group) && Instant::now() < deadline {
        thread::sleep(Duration::from_millis(25));
    }
    assert!(is_gone(process_group), "the leader never exited");
    let descendant: u32 = std::fs::read_to_string(&descendant_pid_file)
        .expect("the leader publishes the descendant pid")
        .trim()
        .parse()
        .expect("numeric descendant pid");
    assert!(!is_gone(descendant), "the descendant must be live");

    let unwound = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        let _guard = guard;
        panic!("simulated assertion failure while the guard is scoped");
    }));
    assert!(unwound.is_err());

    let deadline = Instant::now() + Duration::from_secs(5);
    while !is_gone(descendant) && Instant::now() < deadline {
        thread::sleep(Duration::from_millis(25));
    }
    let survived = !is_gone(descendant);
    // Keep a red run self-cleaning: the group stays alive, so its id is not
    // reusable, while any member is live.
    if survived {
        let _ = nix::sys::signal::kill(
            nix::unistd::Pid::from_raw(-(process_group as i32)),
            nix::sys::signal::Signal::SIGKILL,
        );
    }
    assert!(
        !survived,
        "the guard left a descendant of its process group alive"
    );
}
