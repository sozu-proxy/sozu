//! sozu#1668 end-to-end test: a main upgrade must not cut the requests of a
//! worker that is still draining after its own upgrade.
//!
//! `upgrade --worker 0` leaves the old worker `Stopping`: it finishes its
//! in-flight sessions, then exits. A main upgrade in that window hands the new
//! main every worker it must keep talking to. The draining worker's command
//! channel must be one of them: a worker whose command channel closes returns
//! from its event loop at once (`Server::run`, `lib/src/server.rs`), cutting
//! the requests it was still serving. When the new main skipped `Stopping`
//! workers, the channel closed with the old main and the slow request below
//! was cut.
//!
//! Spawns the real `sozu` binary (`CARGO_BIN_EXE_sozu`) with one worker, an
//! HTTP listener and an in-test backend that answers `/slow` after 8 seconds.
//! It sends `GET /slow`, starts `upgrade --worker 0` once the backend holds the
//! request, runs a main `upgrade` as soon as worker 1 replaced worker 0 (so
//! worker 0 is still draining), and requires the slow request to complete with
//! a `200`.
//!
//! `#[ignore]`d so a contributor's `cargo test` stays fast; CI runs it from its
//! process-level e2e step (`.github/workflows/ci.yml`). Run it by hand with:
//!
//! ```bash
//! cargo test -p sozu --test upgrade_keeps_draining_worker_e2e -- --ignored
//! ```
#![cfg(target_os = "linux")]

use std::{
    io::{Read, Write},
    net::{TcpListener, TcpStream},
    path::Path,
    process::{Child, Command},
    sync::mpsc,
    thread,
    time::{Duration, Instant},
};

/// How long the backend holds `/slow` before answering.
const SLOW_RESPONSE_DELAY: Duration = Duration::from_secs(8);

fn free_port() -> u16 {
    TcpListener::bind("127.0.0.1:0")
        .expect("bind ephemeral port")
        .local_addr()
        .expect("local_addr")
        .port()
}

/// Answer every request with an empty `200`, after `SLOW_RESPONSE_DELAY` for
/// `/slow`. Reports on `slow_received` when a `/slow` request arrives.
fn spawn_backend(slow_received: mpsc::Sender<()>) -> u16 {
    let listener = TcpListener::bind("127.0.0.1:0").expect("bind backend");
    let port = listener.local_addr().expect("backend addr").port();
    thread::spawn(move || {
        for stream in listener.incoming() {
            let Ok(mut stream) = stream else { continue };
            let slow_received = slow_received.clone();
            thread::spawn(move || {
                let mut request = Vec::new();
                let mut buffer = [0u8; 1024];
                while !request.windows(4).any(|w| w == b"\r\n\r\n") {
                    match stream.read(&mut buffer) {
                        Ok(0) | Err(_) => return,
                        Ok(read) => request.extend_from_slice(&buffer[..read]),
                    }
                }
                if request.starts_with(b"GET /slow ") {
                    let _ = slow_received.send(());
                    thread::sleep(SLOW_RESPONSE_DELAY);
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
fn get(port: u16, path: &str, timeout: Duration) -> Vec<u8> {
    let Ok(mut stream) = TcpStream::connect(("127.0.0.1", port)) else {
        return Vec::new();
    };
    let _ = stream.set_read_timeout(Some(timeout));
    let request = format!("GET {path} HTTP/1.1\r\nHost: drain.test\r\nConnection: close\r\n\r\n");
    if stream.write_all(request.as_bytes()).is_err() {
        return Vec::new();
    }
    let mut response = Vec::new();
    let _ = stream.read_to_end(&mut response);
    response
}

fn sozu(config_path: &Path, args: &[&str]) -> std::process::Output {
    Command::new(env!("CARGO_BIN_EXE_sozu"))
        .args(["-c", config_path.to_str().unwrap(), "-t", "30000"])
        .args(args)
        .output()
        .expect("run sozu")
}

/// Worker ids the master reports as running, from `sozu --json status`.
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

fn main_pid(pid_file: &Path) -> Option<u32> {
    std::fs::read_to_string(pid_file).ok()?.trim().parse().ok()
}

fn stop(config_path: &Path, pid_file: &Path, master: &mut Child) {
    let _ = sozu(config_path, &["shutdown"]);
    let deadline = Instant::now() + Duration::from_secs(20);
    while Instant::now() < deadline && master.try_wait().ok().flatten().is_none() {
        thread::sleep(Duration::from_millis(50));
    }
    let _ = master.kill();
    let _ = master.wait();
    if let Some(pid) = main_pid(pid_file) {
        let _ = nix::sys::signal::kill(
            nix::unistd::Pid::from_raw(pid as i32),
            nix::sys::signal::Signal::SIGKILL,
        );
    }
}

#[test]
#[ignore = "process-level: spawns a real master and worker, upgrades them and binds ephemeral ports; run from the dedicated CI step or with --ignored (see module docs)"]
fn main_upgrade_keeps_a_draining_worker_serving_its_requests() {
    let temp = tempfile::tempdir().expect("tempdir");
    let socket_path = temp.path().join("sozu.sock");
    let config_path = temp.path().join("config.toml");
    let pid_file = temp.path().join("sozu.pid");
    let (slow_received_tx, slow_received) = mpsc::channel();
    let backend_port = spawn_backend(slow_received_tx);
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
back_timeout = 60

[clusters.drain]
protocol = "http"
frontends = [ {{ address = "127.0.0.1:{front_port}", hostname = "drain.test" }} ]
backends = [ {{ address = "127.0.0.1:{backend_port}", backend_id = "drain-backend" }} ]
"#,
        socket = socket_path.display(),
        pid_file = pid_file.display(),
    );
    std::fs::write(&config_path, &config).expect("write config");

    let mut master = Command::new(env!("CARGO_BIN_EXE_sozu"))
        .args(["start", "-c", config_path.to_str().unwrap()])
        .spawn()
        .expect("spawn sozu start");

    let ready_by = Instant::now() + Duration::from_secs(20);
    while !get(front_port, "/ready", Duration::from_secs(5)).starts_with(b"HTTP/1.1 200") {
        if Instant::now() > ready_by {
            stop(&config_path, &pid_file, &mut master);
            panic!("the proxy never answered on 127.0.0.1:{front_port}");
        }
        thread::sleep(Duration::from_millis(200));
    }

    let slow = thread::spawn(move || get(front_port, "/slow", Duration::from_secs(40)));
    slow_received
        .recv_timeout(Duration::from_secs(10))
        .expect("the backend never received the slow request");

    // `upgrade --worker 0` only returns once the old worker has drained, so
    // it runs in the background: the main upgrade must land while worker 0 is
    // still `Stopping`, as soon as its replacement (worker 1) runs.
    let mut worker_upgrade = Command::new(env!("CARGO_BIN_EXE_sozu"))
        .args(["-c", config_path.to_str().unwrap(), "-t", "30000"])
        .args(["upgrade", "--worker", "0"])
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
        .spawn()
        .expect("spawn upgrade --worker 0");
    let replaced_by = Instant::now() + Duration::from_secs(20);
    while !running_worker_ids(&config_path).contains(&1) {
        assert!(
            Instant::now() < replaced_by,
            "worker 0 was never replaced by worker 1"
        );
        thread::sleep(Duration::from_millis(50));
    }
    let old_main = main_pid(&pid_file);
    let main_upgrade = sozu(&config_path, &["upgrade"]);
    let deadline = Instant::now() + Duration::from_secs(30);
    let mut old_main_status = None;
    while main_pid(&pid_file) == old_main && Instant::now() < deadline {
        old_main_status = master.try_wait().expect("observe old main");
        thread::sleep(Duration::from_millis(100));
    }
    if old_main_status.is_none() {
        old_main_status = master.try_wait().expect("observe old main after handoff");
    }
    let new_main = main_pid(&pid_file);

    let response = slow.join().expect("slow request thread");
    let client_deadline = Instant::now() + Duration::from_secs(10);
    let mut worker_upgrade_terminal = worker_upgrade
        .try_wait()
        .expect("observe pending worker-upgrade client");
    while worker_upgrade_terminal.is_none() && Instant::now() < client_deadline {
        thread::sleep(Duration::from_millis(50));
        worker_upgrade_terminal = worker_upgrade
            .try_wait()
            .expect("observe pending worker-upgrade client");
    }
    let hit_client_bound = worker_upgrade_terminal.is_none();
    if hit_client_bound {
        let _ = worker_upgrade.kill();
    }
    let worker_upgrade_output = worker_upgrade
        .wait_with_output()
        .expect("collect worker-upgrade client outcome");
    let running_workers = running_worker_ids(&config_path);
    eprintln!(
        "PENDING_UPGRADE_OBSERVATION old_main={old_main:?} old_main_status={old_main_status:?} new_main={new_main:?} worker_upgrade_status={:?} hit_client_bound={hit_client_bound} worker_upgrade_stdout={:?} worker_upgrade_stderr={:?} running_workers={running_workers:?}",
        worker_upgrade_output.status,
        String::from_utf8_lossy(&worker_upgrade_output.stdout),
        String::from_utf8_lossy(&worker_upgrade_output.stderr),
    );
    stop(&config_path, &pid_file, &mut master);

    assert!(
        main_upgrade.status.success(),
        "main upgrade failed: {}",
        String::from_utf8_lossy(&main_upgrade.stderr)
    );
    assert!(
        response.starts_with(b"HTTP/1.1 200"),
        "the request the draining worker was serving must complete across the main upgrade, got {:?}",
        String::from_utf8_lossy(&response)
    );
}
