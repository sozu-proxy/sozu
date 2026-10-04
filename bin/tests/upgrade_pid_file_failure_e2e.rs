//! A main upgrade whose pid file cannot be written must roll back, not stop
//! the proxy.
//!
//! The replacement main publishes its pid by rewriting `pid_file_path`. Once
//! the old main has sent COMMIT it is irreversibly fenced: it exits whatever
//! the replacement does next, and every worker returns from its event loop
//! when its command channel closes. A replacement that only discovered after
//! COMMIT that it could not write the pid file therefore took the whole proxy
//! down, and the old main exited 0, so `Restart=on-failure` did not restart
//! it. Every fallible preparation must happen before PREPARED, where a
//! failure still rolls back to the old main.
//!
//! Spawns the real `sozu` binary (`CARGO_BIN_EXE_sozu`) with one worker and an
//! HTTP listener in front of an in-test backend, replaces the pid file with a
//! directory, then runs `sozu upgrade`. The upgrade must fail, the original
//! main must stay alive, `status` must still list a running worker, and the
//! frontend must keep answering.
//!
//! `#[ignore]`d so a contributor's `cargo test` stays fast; CI runs it from its
//! process-level e2e step (`.github/workflows/ci.yml`). Run it by hand with:
//!
//! ```bash
//! cargo test -p sozu --test upgrade_pid_file_failure_e2e -- --ignored
//! ```
#![cfg(target_os = "linux")]

use std::{
    fs,
    io::{Read, Write},
    net::{TcpListener, TcpStream},
    os::unix::process::CommandExt,
    path::{Path, PathBuf},
    process::{Child, Command, Output},
    thread,
    time::{Duration, Instant},
};

const CONDITION_POLL: Duration = Duration::from_millis(25);

/// Owns the process group of the spawned main, so cleanup reaches its workers
/// and any replacement candidate even after the main itself exited.
struct ProcessGuard {
    config_path: PathBuf,
    process_group: u32,
    main: Child,
}

impl Drop for ProcessGuard {
    fn drop(&mut self) {
        let _ = sozu(&self.config_path, &["shutdown"]);
        let deadline = Instant::now() + Duration::from_secs(10);
        while Instant::now() < deadline
            && process_state(self.process_group).is_some_and(|state| state != 'Z')
        {
            thread::sleep(CONDITION_POLL);
        }
        // The leader is reaped only after this signal, so its PID cannot be
        // reused while it names the process group this test created.
        let _ = nix::sys::signal::kill(
            nix::unistd::Pid::from_raw(-(self.process_group as i32)),
            nix::sys::signal::Signal::SIGKILL,
        );
        let _ = self.main.wait();
    }
}

fn free_port() -> u16 {
    TcpListener::bind("127.0.0.1:0")
        .expect("bind ephemeral port")
        .local_addr()
        .expect("local_addr")
        .port()
}

/// Answer every request with an empty `200`.
fn spawn_backend() -> u16 {
    let listener = TcpListener::bind("127.0.0.1:0").expect("bind backend");
    let port = listener.local_addr().expect("backend addr").port();
    thread::spawn(move || {
        for stream in listener.incoming() {
            let Ok(mut stream) = stream else { continue };
            thread::spawn(move || {
                let mut request = Vec::new();
                let mut buffer = [0u8; 1024];
                while !request.windows(4).any(|w| w == b"\r\n\r\n") {
                    match stream.read(&mut buffer) {
                        Ok(0) | Err(_) => return,
                        Ok(read) => request.extend_from_slice(&buffer[..read]),
                    }
                }
                let _ = stream.write_all(
                    b"HTTP/1.1 200 OK\r\nContent-Length: 0\r\nConnection: close\r\n\r\n",
                );
            });
        }
    });
    port
}

/// One request on its own connection; the raw response bytes.
fn get(port: u16, timeout: Duration) -> Vec<u8> {
    let Ok(mut stream) = TcpStream::connect(("127.0.0.1", port)) else {
        return Vec::new();
    };
    let _ = stream.set_read_timeout(Some(timeout));
    let request = "GET /ready HTTP/1.1\r\nHost: pidfile.test\r\nConnection: close\r\n\r\n";
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

/// Worker ids the main reports as running, from `sozu --json status`.
fn running_worker_ids(config_path: &Path) -> Vec<u64> {
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
        .filter_map(|worker| worker["id"].as_u64())
        .collect()
}

fn process_state(pid: u32) -> Option<char> {
    let stat = fs::read_to_string(format!("/proc/{pid}/stat")).ok()?;
    stat.rsplit_once(") ")?.1.chars().next()
}

#[test]
#[ignore = "process-level: spawns a real master and worker, attempts a main upgrade and binds ephemeral ports; run from the dedicated CI step or with --ignored (see module docs)"]
fn unwritable_pid_file_rolls_back_main_upgrade_and_keeps_serving() {
    let temp = tempfile::tempdir().expect("tempdir");
    let socket_path = temp.path().join("sozu.sock");
    let config_path = temp.path().join("config.toml");
    let pid_file = temp.path().join("sozu.pid");
    let backend_port = spawn_backend();
    let front_port = free_port();

    let config = format!(
        r#"
command_socket = "{socket}"
pid_file_path = "{pid_file}"
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

[clusters.pidfile]
protocol = "http"
frontends = [ {{ address = "127.0.0.1:{front_port}", hostname = "pidfile.test" }} ]
backends = [ {{ address = "127.0.0.1:{backend_port}", backend_id = "pidfile-backend" }} ]
"#,
        socket = socket_path.display(),
        pid_file = pid_file.display(),
    );
    fs::write(&config_path, &config).expect("write config");

    let main = Command::new(env!("CARGO_BIN_EXE_sozu"))
        .args(["start", "-c", config_path.to_str().expect("UTF-8 path")])
        .process_group(0)
        .spawn()
        .expect("spawn sozu start");
    let main_pid = main.id();
    let _guard = ProcessGuard {
        config_path: config_path.clone(),
        process_group: main_pid,
        main,
    };

    let ready_by = Instant::now() + Duration::from_secs(20);
    while !get(front_port, Duration::from_secs(5)).starts_with(b"HTTP/1.1 200") {
        assert!(
            Instant::now() < ready_by,
            "the proxy never answered on 127.0.0.1:{front_port}"
        );
        thread::sleep(Duration::from_millis(200));
    }
    let workers_before = running_worker_ids(&config_path);
    assert!(
        !workers_before.is_empty(),
        "no running worker before the upgrade"
    );

    // The pid file path still names something the original main created, but
    // no process can rewrite it any more.
    fs::remove_file(&pid_file).expect("remove pid file");
    fs::create_dir(&pid_file).expect("replace pid file with a directory");

    let upgrade = sozu(&config_path, &["upgrade"]);

    // Give a dying process group time to finish dying before observing it.
    thread::sleep(Duration::from_secs(1));
    let main_state = process_state(main_pid);
    let status = sozu(&config_path, &["--json", "status"]);
    let workers_after = running_worker_ids(&config_path);
    let front = get(front_port, Duration::from_secs(5));

    let upgrade_stdout = String::from_utf8_lossy(&upgrade.stdout);
    let upgrade_stderr = String::from_utf8_lossy(&upgrade.stderr);
    assert!(
        !upgrade.status.success() && !upgrade_stdout.contains("Upgrade successful"),
        "a main upgrade that cannot write its pid file must fail; status={:?}, stdout={upgrade_stdout:?}, stderr={upgrade_stderr:?}",
        upgrade.status,
    );
    assert!(
        main_state.is_some_and(|state| state != 'Z'),
        "the original main {main_pid} must keep running after the failed upgrade, state={main_state:?}; upgrade stdout={upgrade_stdout:?}, stderr={upgrade_stderr:?}"
    );
    assert!(
        status.status.success(),
        "the original main must still answer status: {}",
        String::from_utf8_lossy(&status.stderr)
    );
    assert_eq!(
        workers_after, workers_before,
        "the original worker must keep running"
    );
    assert!(
        front.starts_with(b"HTTP/1.1 200"),
        "the frontend must keep answering after the failed upgrade, got {:?}",
        String::from_utf8_lossy(&front)
    );
    assert!(
        pid_file.is_dir(),
        "the failed upgrade must not have replaced the pid file path"
    );
}
