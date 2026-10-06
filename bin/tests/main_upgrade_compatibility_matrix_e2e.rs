//! Main-upgrade compatibility across the 2.2.1 / transactional-v2 boundary.
//!
//! The transactional-v2 -> 2.2.1 rejection case starts the sender from a
//! private binary copy, holds a worker upgrade behind an observable backend
//! barrier, and atomically installs the legacy candidate. It requires the
//! sender PID and Hub to remain authoritative, the candidate child to be
//! reaped, and no successful `main_upgraded` audit or boot-generation change.
//!
//! The 2.2.1 -> current case exercises the supported bridge with the current
//! CLI, all three plaintext protocols, saved routing state, and a draining
//! legacy worker.
//!
//! This is a Linux-only, ignored process test. Run it with:
//!
//! ```bash
//! SOZU_MATRIX_LEGACY=/path/to/legacy SOZU_MATRIX_OPTION3=/path/to/option3 \
//! cargo test -p sozu --test main_upgrade_compatibility_matrix_e2e --locked \
//!   -- --ignored --exact --nocapture --test-threads=1
//! ```
//!
//! The forward 2.2.1 -> current compatibility contract has its own positive
//! case. It requires both variables and `curl` on `PATH`, and deliberately
//! fails when any prerequisite is absent. `--insecure` is limited to the
//! repository's self-signed `lolcatho.st` fixture. Select it by exact name:
//!
//! ```bash
//! SOZU_MATRIX_LEGACY=/path/to/sozu-2.2.1 \
//! SOZU_MATRIX_OPTION3=/path/to/current-sozu \
//! cargo test -p sozu --test main_upgrade_compatibility_matrix_e2e --locked \
//!   legacy_2_2_1_upgrades_to_current_and_drains_its_worker \
//!   -- --ignored --exact --nocapture --test-threads=1
//! ```
#![cfg(target_os = "linux")]

use std::{
    collections::{BTreeMap, BTreeSet},
    fs,
    io::{Read, Write},
    net::{TcpListener, TcpStream, UdpSocket},
    os::unix::{fs::PermissionsExt, net::UnixDatagram, process::CommandExt},
    path::{Path, PathBuf},
    process::{Child, Command, Output, Stdio},
    sync::{Arc, Condvar, Mutex, mpsc},
    thread,
    time::{Duration, Instant},
};

use sha2::{Digest, Sha256};

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

fn free_udp_port() -> u16 {
    UdpSocket::bind("127.0.0.1:0")
        .expect("bind ephemeral UDP port")
        .local_addr()
        .expect("ephemeral UDP listener address")
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
                    b"HTTP/1.1 200 OK\r\nContent-Length: 18\r\nConnection: close\r\n\r\ncompatibility-http",
                );
            });
        }
    });
    port
}

fn spawn_tcp_echo_backend() -> u16 {
    let listener = TcpListener::bind("127.0.0.1:0").expect("bind TCP echo backend");
    let port = listener.local_addr().expect("TCP backend address").port();
    thread::spawn(move || {
        for stream in listener.incoming() {
            let Ok(mut stream) = stream else { continue };
            thread::spawn(move || {
                let mut request = Vec::new();
                let mut buffer = [0u8; 1024];
                while !request.ends_with(b"\n") {
                    match stream.read(&mut buffer) {
                        Ok(0) | Err(_) => return,
                        Ok(read) => request.extend_from_slice(&buffer[..read]),
                    }
                }
                let _ = stream.write_all(&request);
            });
        }
    });
    port
}

fn spawn_udp_echo_backend() -> u16 {
    let socket = UdpSocket::bind("127.0.0.1:0").expect("bind UDP echo backend");
    let port = socket.local_addr().expect("UDP backend address").port();
    thread::spawn(move || {
        let mut buffer = [0u8; 1024];
        loop {
            let Ok((read, peer)) = socket.recv_from(&mut buffer) else {
                return;
            };
            let _ = socket.send_to(&buffer[..read], peer);
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

fn assert_http_route(port: u16, path: &str) {
    let response = get(port, path, Duration::from_secs(5));
    assert!(
        response.starts_with(b"HTTP/1.1 200") && response.ends_with(b"compatibility-http"),
        "HTTP route {path} returned the wrong response: {:?}",
        String::from_utf8_lossy(&response)
    );
}

fn assert_https_route(port: u16, path: &str) {
    let resolve = format!("lolcatho.st:{port}:127.0.0.1");
    let url = format!("https://lolcatho.st:{port}{path}");
    let output = Command::new("curl")
        .args([
            "--disable",
            "--http1.1",
            "--insecure",
            "--noproxy",
            "*",
            "--resolve",
            &resolve,
            "--max-time",
            "5",
            "--silent",
            "--show-error",
            "--fail",
            &url,
        ])
        .output()
        .expect("curl is required for the ignored HTTPS compatibility test");
    assert!(
        output.status.success(),
        "HTTPS route {path} failed: stdout={:?} stderr={:?}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    assert_eq!(
        output.stdout, b"compatibility-http",
        "HTTPS route {path} returned the wrong body"
    );
}

fn assert_tcp_route(port: u16, payload: &[u8]) {
    let mut stream = TcpStream::connect(("127.0.0.1", port)).expect("connect to TCP frontend");
    stream
        .set_read_timeout(Some(Duration::from_secs(5)))
        .expect("set TCP read timeout");
    let mut framed_payload = payload.to_vec();
    framed_payload.push(b'\n');
    stream
        .write_all(&framed_payload)
        .expect("write TCP frontend");
    let mut response = vec![0; framed_payload.len()];
    stream
        .read_exact(&mut response)
        .expect("read TCP echo response");
    assert_eq!(
        response, framed_payload,
        "TCP route returned the wrong payload"
    );
}

fn assert_udp_route(port: u16, payload: &[u8]) {
    let socket = UdpSocket::bind("127.0.0.1:0").expect("bind UDP client");
    socket
        .set_read_timeout(Some(Duration::from_secs(5)))
        .expect("set UDP read timeout");
    socket
        .send_to(payload, ("127.0.0.1", port))
        .expect("write UDP frontend");
    let mut response = [0u8; 1024];
    let (read, _) = socket
        .recv_from(&mut response)
        .expect("read UDP echo response");
    assert_eq!(
        &response[..read],
        payload,
        "UDP route returned the wrong payload"
    );
}

fn sha256_file(path: &Path) -> [u8; 32] {
    let mut file = fs::File::open(path).expect("open executable for hashing");
    let mut hasher = Sha256::new();
    let mut buffer = [0u8; 64 * 1024];
    loop {
        let read = file.read(&mut buffer).expect("read executable for hashing");
        if read == 0 {
            break;
        }
        hasher.update(&buffer[..read]);
    }
    hasher.finalize().into()
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

fn state_request_kinds(path: &Path) -> BTreeMap<String, usize> {
    let state = fs::read(path).expect("read saved state");
    let mut kinds = BTreeMap::new();
    for record in state.split(|byte| *byte == 0) {
        let record = std::str::from_utf8(record)
            .expect("saved state must be UTF-8")
            .trim();
        if record.is_empty() {
            continue;
        }
        let value: serde_json::Value =
            serde_json::from_str(record).expect("saved state record must be JSON");
        let request = value["content"]["request_type"]
            .as_object()
            .expect("saved state record must carry a request type");
        assert_eq!(
            request.len(),
            1,
            "saved state record must carry exactly one request type"
        );
        let kind = request.keys().next().expect("one request type").to_owned();
        *kinds.entry(kind).or_default() += 1;
    }
    kinds
}

fn state_route_ids(path: &Path) -> BTreeSet<String> {
    let state = fs::read(path).expect("read saved state");
    let mut ids = BTreeSet::new();
    for record in state.split(|byte| *byte == 0) {
        let record = std::str::from_utf8(record)
            .expect("saved state must be UTF-8")
            .trim();
        if record.is_empty() {
            continue;
        }
        let value: serde_json::Value =
            serde_json::from_str(record).expect("saved state record must be JSON");
        let request = value["content"]["request_type"]
            .as_object()
            .expect("saved state record must carry a request type");
        let payload = request
            .values()
            .next()
            .expect("saved state request must carry a payload");
        for key in ["cluster_id", "backend_id"] {
            if let Some(id) = payload[key].as_str() {
                ids.insert(format!("{key}:{id}"));
            }
        }
    }
    ids
}

fn state_certificate_material_digests(path: &Path) -> BTreeSet<[u8; 32]> {
    let state = fs::read(path).expect("read saved state");
    let mut digests = BTreeSet::new();
    for record in state.split(|byte| *byte == 0) {
        let record = std::str::from_utf8(record)
            .expect("saved state must be UTF-8")
            .trim();
        if record.is_empty() {
            continue;
        }
        let value: serde_json::Value =
            serde_json::from_str(record).expect("saved state record must be JSON");
        let Some(certificate) =
            value["content"]["request_type"]["ADD_CERTIFICATE"].get("certificate")
        else {
            continue;
        };
        let mut hasher = Sha256::new();
        for key in ["certificate", "key"] {
            let material = certificate[key]
                .as_str()
                .unwrap_or_else(|| panic!("ADD_CERTIFICATE must carry {key}"));
            hasher.update(material.len().to_le_bytes());
            hasher.update(material.as_bytes());
        }
        let chain = certificate["certificate_chain"]
            .as_array()
            .expect("ADD_CERTIFICATE must carry a certificate chain");
        for material in chain {
            let material = material
                .as_str()
                .expect("certificate chain entries must be strings");
            hasher.update(material.len().to_le_bytes());
            hasher.update(material.as_bytes());
        }
        digests.insert(hasher.finalize().into());
    }
    digests
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
    // skip visible without `--nocapture`. Unbuffered `writeln!` issues one
    // `write(2)` per format piece, which libtest's own progress output (and
    // the other case, running in parallel) interleaves with. The message is
    // therefore formatted first and written by a single `write_all`, framed by
    // newlines so it never shares a line with a `test … ...` prefix.
    let Some((sender_binary, candidate_binary)) = direction.binaries() else {
        let message = format!(
            "\nskipping {}: set SOZU_MATRIX_LEGACY and SOZU_MATRIX_OPTION3 to the frozen binaries (see module docs)\n",
            direction.label()
        );
        let mut stderr = std::io::stderr().lock();
        let _ = stderr.write_all(message.as_bytes());
        let _ = stderr.flush();
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
        .env_remove("NOTIFY_SOCKET")
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
#[ignore = "process-level positive compatibility case; requires exact 2.2.1 and current binaries"]
fn legacy_2_2_1_upgrades_to_current_and_drains_its_worker() {
    let (legacy_binary, candidate_binary) = Direction::LegacyToOption3
        .binaries()
        .expect("set SOZU_MATRIX_LEGACY and SOZU_MATRIX_OPTION3 to exact executable paths");
    assert!(legacy_binary.is_file(), "legacy binary is missing");
    assert!(candidate_binary.is_file(), "candidate binary is missing");

    let legacy_version = Command::new(&legacy_binary)
        .arg("--version")
        .output()
        .expect("query legacy version");
    let legacy_version = String::from_utf8_lossy(&legacy_version.stdout);
    assert!(
        legacy_version.contains("sozu 2.2.1 (cd02310"),
        "legacy binary must be the cd023104 2.2.1 release, got {legacy_version:?}"
    );

    let temp = tempfile::tempdir().expect("test tempdir");
    let socket_path = temp.path().join("sozu.sock");
    let config_path = temp.path().join("config.toml");
    let pid_file = temp.path().join("sozu.pid");
    let audit_path = temp.path().join("audit.jsonl");
    let installed_binary = temp.path().join("sozu-installed");
    let staged_candidate = temp.path().join("sozu-candidate");
    let before_state = temp.path().join("before.state");
    let after_state = temp.path().join("after.state");
    let assets = Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("../lib/assets")
        .canonicalize()
        .expect("resolve TLS fixture directory");
    let certificate = assets.join("certificate.pem");
    let certificate_chain = assets.join("certificate_chain.pem");
    let key = assets.join("key.pem");

    fs::copy(&legacy_binary, &installed_binary)
        .expect("copy legacy binary to private install path");
    fs::set_permissions(&installed_binary, fs::Permissions::from_mode(0o755))
        .expect("make private legacy binary executable");

    let (held_received_tx, held_received) = mpsc::channel();
    let barrier = BackendBarrier::new();
    let backend_port = spawn_backend(held_received_tx, barrier.clone());
    let front_port = free_port();
    let tcp_backend_port = spawn_tcp_echo_backend();
    let tcp_front_port = loop {
        let port = free_port();
        if port != front_port {
            break port;
        }
    };
    let https_front_port = loop {
        let port = free_port();
        if port != front_port && port != tcp_front_port {
            break port;
        }
    };
    let udp_backend_port = spawn_udp_echo_backend();
    let udp_front_port = loop {
        let port = free_udp_port();
        if port != front_port && port != tcp_front_port && port != https_front_port {
            break port;
        }
    };
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

[[listeners]]
protocol = "tcp"
address = "127.0.0.1:{tcp_front_port}"

[[listeners]]
protocol = "https"
address = "127.0.0.1:{https_front_port}"

[[listeners]]
protocol = "udp"
address = "127.0.0.1:{udp_front_port}"
front_timeout = 1
back_timeout = 1

[clusters.compatibility]
protocol = "http"
frontends = [
  {{ address = "127.0.0.1:{front_port}", hostname = "rejection.test" }},
  {{ address = "127.0.0.1:{https_front_port}", hostname = "lolcatho.st", certificate = "{certificate}", certificate_chain = "{certificate_chain}", key = "{key}" }}
]
backends = [ {{ address = "127.0.0.1:{backend_port}", backend_id = "held-backend" }} ]

[clusters.compatibility_tcp]
protocol = "tcp"
frontends = [ {{ address = "127.0.0.1:{tcp_front_port}" }} ]
backends = [ {{ address = "127.0.0.1:{tcp_backend_port}", backend_id = "tcp-backend" }} ]

[clusters.compatibility_udp]
protocol = "tcp"
frontends = [ {{ address = "127.0.0.1:{udp_front_port}" }} ]
backends = [ {{ address = "127.0.0.1:{udp_backend_port}", backend_id = "udp-backend" }} ]

[clusters.compatibility_udp.udp]
responses = 1
"#,
        socket = socket_path.display(),
        pid_file = pid_file.display(),
        audit = audit_path.display(),
        certificate = certificate.display(),
        certificate_chain = certificate_chain.display(),
        key = key.display(),
    );
    fs::write(&config_path, config).expect("write config");

    let mut start = Command::new(&installed_binary);
    start
        .args([
            "start",
            "-c",
            config_path.to_str().expect("UTF-8 config path"),
        ])
        .env_remove("NOTIFY_SOCKET")
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .process_group(0);
    let main = start.spawn().expect("spawn 2.2.1 main");
    let original_main_pid = main.id();
    let mut guard = ProcessGuard {
        config_path: config_path.clone(),
        client_binary: legacy_binary.clone(),
        barrier: barrier.clone(),
        process_group: original_main_pid,
        original_main: Some(main),
    };

    let ready_deadline = Instant::now() + Duration::from_secs(20);
    while !get(front_port, "/ready", Duration::from_millis(250)).starts_with(b"HTTP/1.1 200") {
        assert!(
            Instant::now() < ready_deadline,
            "2.2.1 proxy never became ready"
        );
        thread::sleep(CONDITION_POLL);
    }
    assert_eq!(main_pid(&pid_file), Some(original_main_pid));
    assert_eq!(
        running_worker_ids(&legacy_binary, &config_path),
        vec![0],
        "2.2.1 must start exactly worker 0"
    );
    assert_http_route(front_port, "/before");
    assert_https_route(https_front_port, "/before");
    assert_tcp_route(tcp_front_port, b"tcp-before");
    assert_udp_route(udp_front_port, b"udp-before");

    let before_save = sozu(
        &legacy_binary,
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
        "2.2.1 state save failed: stdout={:?} stderr={:?}",
        String::from_utf8_lossy(&before_save.stdout),
        String::from_utf8_lossy(&before_save.stderr)
    );
    let before_kinds = state_request_kinds(&before_state);
    for expected in [
        "ACTIVATE_LISTENER",
        "ADD_BACKEND",
        "ADD_CLUSTER",
        "ADD_CERTIFICATE",
        "ADD_HTTP_FRONTEND",
        "ADD_HTTP_LISTENER",
        "ADD_HTTPS_FRONTEND",
        "ADD_HTTPS_LISTENER",
        "ADD_TCP_FRONTEND",
        "ADD_TCP_LISTENER",
        "ADD_UDP_FRONTEND",
        "ADD_UDP_LISTENER",
    ] {
        assert!(
            before_kinds.contains_key(expected),
            "2.2.1 state lacks {expected}: {before_kinds:?}"
        );
    }
    let before_route_ids = state_route_ids(&before_state);
    let expected_route_ids = BTreeSet::from([
        "backend_id:held-backend".to_owned(),
        "backend_id:tcp-backend".to_owned(),
        "backend_id:udp-backend".to_owned(),
        "cluster_id:compatibility".to_owned(),
        "cluster_id:compatibility_tcp".to_owned(),
        "cluster_id:compatibility_udp".to_owned(),
    ]);
    assert_eq!(
        before_route_ids, expected_route_ids,
        "2.2.1 saved state must identify every exercised route"
    );
    let before_certificates = state_certificate_material_digests(&before_state);
    assert_eq!(
        before_certificates.len(),
        1,
        "2.2.1 saved state must carry the HTTPS certificate material"
    );

    let held = thread::spawn(move || get(front_port, "/held", Duration::from_secs(40)));
    held_received
        .recv_timeout(Duration::from_secs(10))
        .expect("backend never observed held request");

    fs::copy(&candidate_binary, &staged_candidate).expect("stage current candidate binary");
    fs::set_permissions(&staged_candidate, fs::Permissions::from_mode(0o755))
        .expect("make current candidate executable");
    fs::rename(&staged_candidate, &installed_binary).expect("atomically install current candidate");

    // The current CLI asks the 2.2.1 main to upgrade itself. That old main
    // supplies the flat legacy payload and old internal argv to the installed
    // candidate; the current CLI then reports the aggregate worker result.
    let mut upgrade = Command::new(&candidate_binary)
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
        .expect("spawn current upgrade client");

    let main_deadline = Instant::now() + Duration::from_secs(20);
    let new_main_pid = loop {
        if let Some(pid) = main_pid(&pid_file)
            && pid != original_main_pid
            && process_state(pid).is_some_and(|state| state != 'Z')
        {
            break pid;
        }
        assert!(
            Instant::now() < main_deadline,
            "2.2.1 main was not replaced by the current candidate"
        );
        thread::sleep(CONDITION_POLL);
    };
    assert!(
        process_state(original_main_pid).is_none_or(|state| state == 'Z'),
        "candidate published pid {new_main_pid} while legacy parent {original_main_pid} was still alive"
    );
    assert_eq!(
        sha256_file(Path::new(&format!("/proc/{new_main_pid}/exe"))),
        sha256_file(&candidate_binary),
        "new main PID does not execute the supplied current candidate"
    );

    let worker_deadline = Instant::now() + Duration::from_secs(20);
    while !running_worker_ids(&candidate_binary, &config_path).contains(&1) {
        assert!(
            Instant::now() < worker_deadline,
            "current main never replaced legacy worker 0 with worker 1"
        );
        thread::sleep(CONDITION_POLL);
    }
    assert!(
        upgrade
            .try_wait()
            .expect("observe upgrade client")
            .is_none(),
        "full upgrade completed before the held request let worker 0 drain"
    );
    assert_http_route(front_port, "/during");
    assert_https_route(https_front_port, "/during");
    assert_tcp_route(tcp_front_port, b"tcp-during");
    assert_udp_route(udp_front_port, b"udp-during");

    barrier.release();
    let held_response = held.join().expect("held request thread");
    let upgrade_deadline = Instant::now() + Duration::from_secs(15);
    while upgrade
        .try_wait()
        .expect("observe terminal upgrade client")
        .is_none()
        && Instant::now() < upgrade_deadline
    {
        thread::sleep(CONDITION_POLL);
    }
    let upgrade_timed_out = upgrade
        .try_wait()
        .expect("observe upgrade client at deadline")
        .is_none();
    if upgrade_timed_out {
        let _ = upgrade.kill();
    }
    let upgrade_output = upgrade.wait_with_output().expect("collect upgrade output");

    assert!(
        held_response.starts_with(b"HTTP/1.1 200"),
        "legacy worker dropped its held request while draining: {:?}",
        String::from_utf8_lossy(&held_response)
    );
    assert!(
        !upgrade_timed_out && upgrade_output.status.success(),
        "2.2.1 -> current full upgrade failed: status={:?} stdout={:?} stderr={:?}",
        upgrade_output.status,
        String::from_utf8_lossy(&upgrade_output.stdout),
        String::from_utf8_lossy(&upgrade_output.stderr)
    );
    assert_eq!(
        running_worker_ids(&candidate_binary, &config_path),
        vec![1],
        "only replacement worker 1 must remain running"
    );

    let after_save = sozu(
        &candidate_binary,
        &config_path,
        &[
            "state",
            "save",
            "-f",
            after_state.to_str().expect("UTF-8 state path"),
        ],
    );
    assert!(
        after_save.status.success(),
        "post-upgrade state save failed: stdout={:?} stderr={:?}",
        String::from_utf8_lossy(&after_save.stdout),
        String::from_utf8_lossy(&after_save.stderr)
    );
    assert_eq!(
        state_request_kinds(&after_state),
        before_kinds,
        "main and worker upgrade changed the persisted routing-state shape"
    );
    assert_eq!(
        state_route_ids(&after_state),
        before_route_ids,
        "main and worker upgrade changed the persisted route identities"
    );
    assert_eq!(
        state_certificate_material_digests(&after_state),
        before_certificates,
        "main and worker upgrade changed the persisted HTTPS certificate material"
    );
    assert_http_route(front_port, "/after");
    assert_https_route(https_front_port, "/after");
    assert_tcp_route(tcp_front_port, b"tcp-after");
    assert_udp_route(udp_front_port, b"udp-after");

    let _ = sozu(&candidate_binary, &config_path, &["shutdown"]);
    if let Some(mut main) = guard.original_main.take() {
        finish_owned_process_group(guard.process_group, &mut main, Duration::from_secs(10));
    }
}

#[test]
#[ignore = "process-level failed-candidate regression; requires exact 2.2.1 and current binaries"]
fn legacy_2_2_1_preserves_authority_when_candidate_exits_nonzero() {
    let (legacy_binary, candidate_binary) = Direction::LegacyToOption3
        .binaries()
        .expect("set SOZU_MATRIX_LEGACY and SOZU_MATRIX_OPTION3 to exact executable paths");
    assert!(legacy_binary.is_file(), "legacy binary is missing");
    assert!(candidate_binary.is_file(), "candidate binary is missing");

    let temp = tempfile::tempdir().expect("test tempdir");
    let socket_path = temp.path().join("sozu.sock");
    let config_path = temp.path().join("config.toml");
    let pid_file = temp.path().join("sozu.pid");
    let installed_binary = temp.path().join("sozu-installed");
    let staged_candidate = temp.path().join("sozu-candidate");

    fs::copy(&legacy_binary, &installed_binary)
        .expect("copy legacy binary to private install path");
    fs::set_permissions(&installed_binary, fs::Permissions::from_mode(0o755))
        .expect("make private legacy binary executable");

    let (backend_tx, _backend_rx) = mpsc::channel();
    let barrier = BackendBarrier::new();
    let backend_port = spawn_backend(backend_tx, barrier.clone());
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

[clusters.compatibility]
protocol = "http"
frontends = [ {{ address = "127.0.0.1:{front_port}", hostname = "rejection.test" }} ]
backends = [ {{ address = "127.0.0.1:{backend_port}", backend_id = "backend" }} ]
"#,
        socket = socket_path.display(),
        pid_file = pid_file.display(),
    );
    fs::write(&config_path, config).expect("write config");

    let mut start = Command::new(&installed_binary);
    start
        .args([
            "start",
            "-c",
            config_path.to_str().expect("UTF-8 config path"),
        ])
        .env_remove("NOTIFY_SOCKET")
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .process_group(0);
    let main = start.spawn().expect("spawn 2.2.1 main");
    let original_main_pid = main.id();
    let mut guard = ProcessGuard {
        config_path: config_path.clone(),
        client_binary: legacy_binary.clone(),
        barrier,
        process_group: original_main_pid,
        original_main: Some(main),
    };

    let ready_deadline = Instant::now() + Duration::from_secs(20);
    while !get(front_port, "/ready", Duration::from_millis(250)).starts_with(b"HTTP/1.1 200") {
        assert!(
            Instant::now() < ready_deadline,
            "2.2.1 proxy never became ready"
        );
        thread::sleep(CONDITION_POLL);
    }
    assert_eq!(main_pid(&pid_file), Some(original_main_pid));

    fs::write(&staged_candidate, b"#!/bin/sh\nexit 42\n").expect("write non-zero candidate");
    fs::set_permissions(&staged_candidate, fs::Permissions::from_mode(0o755))
        .expect("make non-zero candidate executable");
    fs::rename(&staged_candidate, &installed_binary).expect("install non-zero candidate");

    let upgrade = sozu(&candidate_binary, &config_path, &["upgrade"]);
    assert!(
        !upgrade.status.success(),
        "current CLI must report the candidate's non-zero exit: stdout={:?} stderr={:?}",
        String::from_utf8_lossy(&upgrade.stdout),
        String::from_utf8_lossy(&upgrade.stderr)
    );
    assert_eq!(
        main_pid(&pid_file),
        Some(original_main_pid),
        "failed candidate changed the authoritative main"
    );
    assert!(
        process_state(original_main_pid).is_some_and(|state| state != 'Z'),
        "failed candidate killed the legacy main"
    );
    assert_http_route(front_port, "/after-nonzero-candidate");

    let _ = sozu(&legacy_binary, &config_path, &["shutdown"]);
    if let Some(mut main) = guard.original_main.take() {
        finish_owned_process_group(guard.process_group, &mut main, Duration::from_secs(10));
    }
}

#[test]
#[ignore = "process-level supervised compatibility refusal; requires exact 2.2.1 and current binaries"]
fn legacy_2_2_1_upgrade_refuses_notify_supervision_before_ack() {
    let (legacy_binary, candidate_binary) = Direction::LegacyToOption3
        .binaries()
        .expect("set SOZU_MATRIX_LEGACY and SOZU_MATRIX_OPTION3 to exact executable paths");
    assert!(legacy_binary.is_file(), "legacy binary is missing");
    assert!(candidate_binary.is_file(), "candidate binary is missing");

    let temp = tempfile::tempdir().expect("test tempdir");
    let socket_path = temp.path().join("sozu.sock");
    let notify_path = temp.path().join("notify.sock");
    let config_path = temp.path().join("config.toml");
    let pid_file = temp.path().join("sozu.pid");
    let installed_binary = temp.path().join("sozu-installed");
    let staged_candidate = temp.path().join("sozu-candidate");
    let _notify_socket = UnixDatagram::bind(&notify_path).expect("bind fake systemd notify socket");

    fs::copy(&legacy_binary, &installed_binary)
        .expect("copy legacy binary to private install path");
    fs::set_permissions(&installed_binary, fs::Permissions::from_mode(0o755))
        .expect("make private legacy binary executable");

    let (backend_tx, _backend_rx) = mpsc::channel();
    let barrier = BackendBarrier::new();
    let backend_port = spawn_backend(backend_tx, barrier.clone());
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

[clusters.compatibility]
protocol = "http"
frontends = [ {{ address = "127.0.0.1:{front_port}", hostname = "rejection.test" }} ]
backends = [ {{ address = "127.0.0.1:{backend_port}", backend_id = "backend" }} ]
"#,
        socket = socket_path.display(),
        pid_file = pid_file.display(),
    );
    fs::write(&config_path, config).expect("write config");

    let mut start = Command::new(&installed_binary);
    start
        .args([
            "start",
            "-c",
            config_path.to_str().expect("UTF-8 config path"),
        ])
        .env("NOTIFY_SOCKET", &notify_path)
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .process_group(0);
    let main = start.spawn().expect("spawn supervised 2.2.1 main");
    let original_main_pid = main.id();
    let mut guard = ProcessGuard {
        config_path: config_path.clone(),
        client_binary: legacy_binary.clone(),
        barrier,
        process_group: original_main_pid,
        original_main: Some(main),
    };

    let ready_deadline = Instant::now() + Duration::from_secs(20);
    while !get(front_port, "/ready", Duration::from_millis(250)).starts_with(b"HTTP/1.1 200") {
        assert!(
            Instant::now() < ready_deadline,
            "supervised 2.2.1 proxy never became ready"
        );
        thread::sleep(CONDITION_POLL);
    }
    assert_eq!(main_pid(&pid_file), Some(original_main_pid));

    fs::copy(&candidate_binary, &staged_candidate).expect("stage current candidate binary");
    fs::set_permissions(&staged_candidate, fs::Permissions::from_mode(0o755))
        .expect("make current candidate executable");
    fs::rename(&staged_candidate, &installed_binary).expect("atomically install current candidate");

    let upgrade = sozu(&candidate_binary, &config_path, &["upgrade"]);
    assert!(
        !upgrade.status.success(),
        "legacy bridge must refuse notify supervision before ACK: stdout={:?} stderr={:?}",
        String::from_utf8_lossy(&upgrade.stdout),
        String::from_utf8_lossy(&upgrade.stderr)
    );
    assert_eq!(
        main_pid(&pid_file),
        Some(original_main_pid),
        "refused bridge changed the authoritative main"
    );
    assert!(
        process_state(original_main_pid).is_some_and(|state| state != 'Z'),
        "refused bridge killed the legacy main"
    );
    assert!(
        get(front_port, "/after-refusal", Duration::from_secs(5)).starts_with(b"HTTP/1.1 200"),
        "legacy main stopped serving after supervised bridge refusal"
    );

    let _ = sozu(&legacy_binary, &config_path, &["shutdown"]);
    if let Some(mut main) = guard.original_main.take() {
        finish_owned_process_group(guard.process_group, &mut main, Duration::from_secs(10));
    }
}
