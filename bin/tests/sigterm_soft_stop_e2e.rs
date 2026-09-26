//! sozu#1555 end-to-end test: `SIGTERM` to every process, as `systemctl stop`
//! sends it to a unit without `ExecStop=`, must stop the workers through the
//! soft-stop path, so the access-log records a `file://` target still buffers
//! reach the file.
//!
//! Before the fix neither process handled the signal: both died on the spot,
//! the worker with its log buffer unflushed (6 of 27 records lost), and the
//! master reported death by signal instead of a clean exit.
//!
//! Spawns the real `sozu` binary (`CARGO_BIN_EXE_sozu`) with one worker, an
//! HTTP listener and a `file://` access log, proxies requests to an in-test
//! backend, then signals the master first and the worker second, in the order
//! systemd uses. It asks the master for its worker (`sozu --json status`)
//! rather than reading `/proc/<pid>/task/<pid>/children`, which exists only on
//! kernels built with `CONFIG_PROC_CHILDREN`. Linux-only: it watches the
//! worker leave through `/proc/<pid>/stat`.
//!
//! `#[ignore]`d so a contributor's `cargo test` stays fast; CI runs it from its
//! process-level e2e step (`.github/workflows/ci.yml`). Run it by hand with:
//!
//! ```bash
//! cargo test -p sozu --test sigterm_soft_stop_e2e -- --ignored
//! ```
#![cfg(target_os = "linux")]

use std::{
    io::{Read, Write},
    net::{TcpListener, TcpStream},
    process::{Child, Command},
    thread,
    time::{Duration, Instant},
};

use nix::{
    sys::signal::{Signal, kill},
    unistd::Pid,
};

/// Enough records to leave some in the 4096-byte buffer of a `file://` target.
const REQUESTS: usize = 27;

fn free_port() -> u16 {
    TcpListener::bind("127.0.0.1:0")
        .expect("bind ephemeral port")
        .local_addr()
        .expect("local_addr")
        .port()
}

/// Answer every request with an empty `200` and close the connection.
fn spawn_backend() -> u16 {
    let listener = TcpListener::bind("127.0.0.1:0").expect("bind backend");
    let port = listener.local_addr().expect("backend addr").port();
    thread::spawn(move || {
        for stream in listener.incoming() {
            let Ok(mut stream) = stream else { continue };
            let mut request = Vec::new();
            let mut buffer = [0u8; 1024];
            while !request.windows(4).any(|w| w == b"\r\n\r\n") {
                match stream.read(&mut buffer) {
                    Ok(0) | Err(_) => break,
                    Ok(read) => request.extend_from_slice(&buffer[..read]),
                }
            }
            let _ = stream
                .write_all(b"HTTP/1.1 200 OK\r\nContent-Length: 0\r\nConnection: close\r\n\r\n");
        }
    });
    port
}

/// One request on its own connection; `true` on a `200`.
fn get(port: u16, path: &str) -> bool {
    let Ok(mut stream) = TcpStream::connect(("127.0.0.1", port)) else {
        return false;
    };
    let _ = stream.set_read_timeout(Some(Duration::from_secs(5)));
    let request = format!("GET {path} HTTP/1.1\r\nHost: sigterm.test\r\nConnection: close\r\n\r\n");
    if stream.write_all(request.as_bytes()).is_err() {
        return false;
    }
    let mut response = Vec::new();
    let _ = stream.read_to_end(&mut response);
    response.starts_with(b"HTTP/1.1 200")
}

/// Pids of the workers the master reports as running, from `sozu --json
/// status` over its command socket. Empty when the master does not answer yet.
fn running_workers(config_path: &std::path::Path) -> Vec<u32> {
    let Ok(output) = Command::new(env!("CARGO_BIN_EXE_sozu"))
        .args(["-c", config_path.to_str().unwrap(), "--json", "status"])
        .output()
    else {
        return Vec::new();
    };
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

fn wait_with_deadline(master: &mut Child, deadline: Duration) -> Option<std::process::ExitStatus> {
    let end = Instant::now() + deadline;
    while Instant::now() < end {
        if let Some(status) = master.try_wait().expect("try_wait") {
            return Some(status);
        }
        thread::sleep(Duration::from_millis(50));
    }
    None
}

#[test]
#[ignore = "process-level: spawns a real master and worker and binds ephemeral ports; run from the dedicated CI step or with --ignored (see module docs)"]
fn sigterm_to_master_and_worker_flushes_every_access_log_record() {
    let temp = tempfile::tempdir().expect("tempdir");
    let socket_path = temp.path().join("sozu.sock");
    let config_path = temp.path().join("config.toml");
    let access_log = temp.path().join("access.log");
    let backend_port = spawn_backend();
    let front_port = free_port();

    let config = format!(
        r#"
command_socket = "{socket}"
command_buffer_size = 16384
max_command_buffer_size = 163840
worker_count = 1
worker_automatic_restart = false
handle_process_affinity = false
log_level = "info"
log_target = "stderr"
access_logs_target = "file://{access_log}"
max_connections = 100
activate_listeners = true

[[listeners]]
protocol = "http"
address = "127.0.0.1:{front_port}"

[clusters.sigterm]
protocol = "http"
frontends = [ {{ address = "127.0.0.1:{front_port}", hostname = "sigterm.test" }} ]
backends = [ {{ address = "127.0.0.1:{backend_port}", backend_id = "sigterm-backend" }} ]
"#,
        socket = socket_path.display(),
        access_log = access_log.display(),
    );
    std::fs::write(&config_path, &config).expect("write config");

    let mut master = Command::new(env!("CARGO_BIN_EXE_sozu"))
        .args(["start", "-c", config_path.to_str().unwrap()])
        .spawn()
        .expect("spawn sozu start");
    let master_pid = master.id();

    let ready_by = Instant::now() + Duration::from_secs(20);
    while !get(front_port, "/ready") {
        if Instant::now() > ready_by {
            let _ = master.kill();
            let _ = master.wait();
            panic!("the proxy never answered on 127.0.0.1:{front_port}");
        }
        thread::sleep(Duration::from_millis(200));
    }
    let mut workers = running_workers(&config_path);
    let listed_by = Instant::now() + Duration::from_secs(10);
    while workers.is_empty() && Instant::now() < listed_by {
        thread::sleep(Duration::from_millis(100));
        workers = running_workers(&config_path);
    }
    assert_eq!(workers.len(), 1, "expected one worker, found {workers:?}");
    let worker_pid = workers[0];

    for index in 1..=REQUESTS {
        assert!(
            get(front_port, &format!("/req-{index}")),
            "request {index} failed"
        );
    }

    // systemd's order for `KillMode=control-group`: the main process, then the
    // rest of the control group.
    kill(Pid::from_raw(master_pid as i32), Signal::SIGTERM).expect("SIGTERM master");
    kill(Pid::from_raw(worker_pid as i32), Signal::SIGTERM).expect("SIGTERM worker");

    let status = wait_with_deadline(&mut master, Duration::from_secs(20));
    if status.is_none() {
        let _ = kill(Pid::from_raw(worker_pid as i32), Signal::SIGKILL);
        let _ = master.kill();
        let _ = master.wait();
    }
    // The worker may outlive the master by a moment: wait for it to go.
    let gone_by = Instant::now() + Duration::from_secs(10);
    while Instant::now() < gone_by
        && std::fs::read_to_string(format!("/proc/{worker_pid}/stat"))
            .is_ok_and(|stat| !stat.contains(") Z "))
    {
        thread::sleep(Duration::from_millis(50));
    }

    let logged = std::fs::read_to_string(&access_log).unwrap_or_default();
    let missing: Vec<usize> = (1..=REQUESTS)
        .filter(|index| !logged.contains(&format!("/req-{index} ")))
        .collect();
    assert!(
        missing.is_empty(),
        "access-log records lost on SIGTERM: {missing:?} of {REQUESTS}"
    );
    let status = status.expect("the master did not exit within 20 s of SIGTERM");
    assert!(
        status.success(),
        "the master must stop cleanly on SIGTERM, got {status:?}"
    );
}
