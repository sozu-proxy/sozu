//! sozu#1671 end-to-end test: a soft `shutdown` issued while a worker drains a
//! slow request must report the final outcome of the stop, not a CLI timeout.
//!
//! The main answers a `SoftStop` with a `Processing` at once, then sends the
//! final `Ok` only once every worker has drained (`StopTask::on_finish`,
//! `bin/src/command/requests.rs`). The drain lasts as long as the longest
//! in-flight session. When the CLI read that answer with the default 1 s
//! `ctl_command_timeout`, it failed with `TimeoutReached(1s)` and disconnected
//! while the shutdown went on and succeeded.
//!
//! Spawns the real `sozu` binary (`CARGO_BIN_EXE_sozu`) with one worker, an
//! HTTP listener and an in-test backend that answers `/slow` after 8 seconds.
//! It sends `GET /slow`, runs `shutdown` with the default timeout once the
//! backend holds the request, and requires the CLI to succeed with the final
//! stop message, the slow request to complete with a `200` and the main to
//! exit.
//!
//! `#[ignore]`d so a contributor's `cargo test` stays fast; CI runs it from its
//! process-level e2e step (`.github/workflows/ci.yml`). Run it by hand with:
//!
//! ```bash
//! cargo test -p sozu --test soft_stop_waits_for_drain_e2e -- --ignored
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

/// How long the backend holds `/slow` before answering: well past the default
/// 1 s `ctl_command_timeout`.
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

fn main_pid(pid_file: &Path) -> Option<u32> {
    std::fs::read_to_string(pid_file).ok()?.trim().parse().ok()
}

/// Wait for the master to exit, then make sure nothing survives the test.
fn reap(pid_file: &Path, master: &mut Child, deadline: Instant) -> bool {
    while Instant::now() < deadline {
        if master.try_wait().ok().flatten().is_some() {
            return true;
        }
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
    false
}

#[test]
#[ignore = "process-level: spawns a real master and worker and binds ephemeral ports; run from the dedicated CI step or with --ignored (see module docs)"]
fn soft_shutdown_reports_success_after_the_drain() {
    let temp = tempfile::tempdir().expect("tempdir");
    let socket_path = temp.path().join("sozu.sock");
    let config_path = temp.path().join("config.toml");
    let pid_file = temp.path().join("sozu.pid");
    let (slow_received_tx, slow_received) = mpsc::channel();
    let backend_port = spawn_backend(slow_received_tx);
    let front_port = free_port();

    // No `ctl_command_timeout`: the CLI runs with its 1 s default.
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
            reap(&pid_file, &mut master, Instant::now());
            panic!("the proxy never answered on 127.0.0.1:{front_port}");
        }
        thread::sleep(Duration::from_millis(200));
    }

    let slow = thread::spawn(move || get(front_port, "/slow", Duration::from_secs(40)));
    slow_received
        .recv_timeout(Duration::from_secs(10))
        .expect("the backend never received the slow request");

    let shutdown = Command::new(env!("CARGO_BIN_EXE_sozu"))
        .args(["-c", config_path.to_str().unwrap(), "shutdown"])
        .output()
        .expect("run sozu shutdown");

    let response = slow.join().expect("slow request thread");
    let exited = reap(
        &pid_file,
        &mut master,
        Instant::now() + Duration::from_secs(20),
    );

    let stdout = String::from_utf8_lossy(&shutdown.stdout);
    assert!(
        shutdown.status.success(),
        "a soft shutdown must report the outcome of the drain, not a CLI timeout: stdout {stdout:?}, stderr {:?}",
        String::from_utf8_lossy(&shutdown.stderr)
    );
    assert!(
        stdout.contains("Successfully closed 1 workers, 0 errors"),
        "the CLI must print the final stop message, got {stdout:?}"
    );
    assert!(
        response.starts_with(b"HTTP/1.1 200"),
        "the request the worker was draining must complete, got {:?}",
        String::from_utf8_lossy(&response)
    );
    assert!(exited, "the main process must exit after the soft stop");
}
