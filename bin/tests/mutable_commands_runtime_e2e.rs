//! Real-process coverage for mutable Sōzu commands.
//!
//! The current cases drive a real main process and workers through the
//! production command socket. They cover backend changes, HTTP frontend and
//! listener changes, listener reconstruction for every proxy protocol,
//! certificate refusals, activation refusal, and mixed-worker timeout paths.
//! They observe SaveState, per-process QueryCluster or certificate state, and
//! real HTTP, verified TLS, TCP, and UDP traffic.
//!
//! The suite does not promise fleet-wide atomicity. Its mixed-worker cases
//! observe the explicit failure, every process state, and the actual late-order
//! or worker-replay recovery path before claiming convergence.
//!
//! Linux process signals and a real Sōzu binary make this an ignored test;
//! the `proc-observer-e2e` job of `.github/workflows/ci.yml` runs it in every
//! provider cell, on a GitHub-hosted VM as root of a user namespace, because
//! it observes the hardened main through `/proc/<pid>/...`.
//! The published-2.3.1 upgrade boundary additionally needs the frozen release
//! binary named by `SOZU_CERTIFICATE_METADATA_LEGACY`. Generic CI provider
//! runs omit that external-artifact case; a mandatory crypto-ring step executes
//! it separately with the verified release binary. A direct suite invocation
//! without the variable prints a visible skip for that case.
//!
//! ```bash
//! cargo test -p sozu --locked --test mutable_commands_runtime_e2e -- --ignored --nocapture
//! ```
#![cfg(target_os = "linux")]

use std::{
    collections::{BTreeMap, BTreeSet},
    fs::File,
    io::{ErrorKind, Read, Write},
    net::{SocketAddr, TcpListener, TcpStream, UdpSocket},
    os::unix::{
        fs::{MetadataExt, PermissionsExt},
        process::CommandExt,
    },
    path::{Path, PathBuf},
    process::{Child, Command, Stdio},
    sync::{
        Arc,
        atomic::{AtomicBool, Ordering},
    },
    thread::{self, JoinHandle},
    time::{Duration, Instant},
};

use nix::{
    sys::signal::{Signal, kill},
    unistd::{Pid, getpgid},
};
use sozu_command_lib::{
    channel::Channel,
    config::ListenerBuilder,
    parser::parse_several_requests,
    proto::command::{
        ActivateListener, AddBackend, AddCertificate, CertificateAndKey, Cluster,
        ClusterInformation, DeactivateListener, HttpListenerConfig, ListWorkers, ListenerType,
        PathRule, PathRuleKind, QueryCertificatesFilters, QueryMetricsOptions, RemoveBackend,
        RemoveCertificate, RemoveListener, ReplaceCertificate, Request, RequestHttpFrontend,
        RequestTcpFrontend, RequestUdpFrontend, Response, ResponseStatus, RulePosition,
        SocketAddress, UpdateHttpListenerConfig, UpgradeMain, WorkerRequest,
        filtered_metrics::Inner, request::RequestType, response_content::ContentType,
    },
    state::ConfigState,
};

const COMMAND_BUFFER_SIZE: u64 = 16_384;
const MAX_COMMAND_BUFFER_SIZE: u64 = 163_840;
const COMMAND_TIMEOUT: Duration = Duration::from_secs(15);
const CONDITION_POLL: Duration = Duration::from_millis(25);
const TLS_PROBE_TIMEOUT: Duration = Duration::from_secs(4);
const HOST: &str = "127.0.0.1";
const HTTP_HOST: &str = "mutable-http.test";
const HTTPS_HOST: &str = "lolcatho.st";
const TLS_MIN_EXPIRY_METRIC: &str = "tls.cert.min_expires_at_seconds";

fn sozu_bin() -> &'static str {
    env!("CARGO_BIN_EXE_sozu")
}

/// Real-main launcher: a same-UID observer cannot read `/proc/<pid>/exe` of
/// the hardened (non-dumpable) main. `unshare --user --map-current-user` keeps
/// the child PID and UID while making the owning observer able to read them.
/// Absence or refusal of `unshare` fails the spawn visibly; there is no fallback.
fn observable_main_command(binary: &Path) -> Command {
    let mut command = Command::new("unshare");
    command
        .args(["--user", "--map-current-user", "--"])
        .arg(binary);
    command
}

fn free_tcp_port() -> u16 {
    TcpListener::bind((HOST, 0))
        .expect("bind ephemeral TCP port")
        .local_addr()
        .expect("read ephemeral TCP port")
        .port()
}

fn free_udp_port() -> u16 {
    UdpSocket::bind((HOST, 0))
        .expect("bind ephemeral UDP port")
        .local_addr()
        .expect("read ephemeral UDP port")
        .port()
}

fn wait_until<T>(description: &str, timeout: Duration, mut probe: impl FnMut() -> Option<T>) -> T {
    let deadline = Instant::now() + timeout;
    while Instant::now() < deadline {
        if let Some(value) = probe() {
            return value;
        }
        thread::sleep(CONDITION_POLL);
    }
    panic!("timed out after {timeout:?} waiting for {description}");
}

struct BackendThread {
    stop: Arc<AtomicBool>,
    wake_tcp: Option<SocketAddr>,
    wake_udp: Option<SocketAddr>,
    handle: Option<JoinHandle<()>>,
}

impl Drop for BackendThread {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::Release);
        if let Some(address) = self.wake_tcp {
            let _ = TcpStream::connect_timeout(&address, Duration::from_millis(100));
        }
        if let Some(address) = self.wake_udp
            && let Ok(socket) = UdpSocket::bind((HOST, 0))
        {
            let _ = socket.send_to(b"stop", address);
        }
        if let Some(handle) = self.handle.take() {
            handle.join().expect("join owned backend thread");
        }
    }
}

fn spawn_http_backend(label: &'static str) -> (u16, BackendThread) {
    let listener = TcpListener::bind((HOST, 0)).expect("bind HTTP backend");
    listener
        .set_nonblocking(true)
        .expect("set HTTP backend nonblocking");
    let address = listener.local_addr().expect("HTTP backend address");
    let stop = Arc::new(AtomicBool::new(false));
    let thread_stop = Arc::clone(&stop);
    let handle = thread::spawn(move || {
        while !thread_stop.load(Ordering::Acquire) {
            match listener.accept() {
                Ok((mut stream, _)) => {
                    let _ = stream.set_read_timeout(Some(Duration::from_secs(2)));
                    let mut request = Vec::new();
                    let mut chunk = [0u8; 1024];
                    while !request.windows(4).any(|window| window == b"\r\n\r\n") {
                        match stream.read(&mut chunk) {
                            Ok(0) | Err(_) => break,
                            Ok(read) => request.extend_from_slice(&chunk[..read]),
                        }
                    }
                    let body = format!("{label}\n");
                    let response = format!(
                        "HTTP/1.1 200 OK\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
                        body.len()
                    );
                    let _ = stream.write_all(response.as_bytes());
                }
                Err(error) if error.kind() == ErrorKind::WouldBlock => {
                    thread::sleep(CONDITION_POLL);
                }
                Err(_) => return,
            }
        }
    });
    (
        address.port(),
        BackendThread {
            stop,
            wake_tcp: Some(address),
            wake_udp: None,
            handle: Some(handle),
        },
    )
}

fn spawn_tcp_backend() -> (u16, BackendThread) {
    let listener = TcpListener::bind((HOST, 0)).expect("bind TCP backend");
    listener
        .set_nonblocking(true)
        .expect("set TCP backend nonblocking");
    let address = listener.local_addr().expect("TCP backend address");
    let stop = Arc::new(AtomicBool::new(false));
    let thread_stop = Arc::clone(&stop);
    let handle = thread::spawn(move || {
        while !thread_stop.load(Ordering::Acquire) {
            match listener.accept() {
                Ok((mut stream, _)) => {
                    let _ = stream.set_read_timeout(Some(Duration::from_secs(2)));
                    let mut bytes = [0u8; 128];
                    if let Ok(read) = stream.read(&mut bytes) {
                        let _ = stream.write_all(&bytes[..read]);
                    }
                }
                Err(error) if error.kind() == ErrorKind::WouldBlock => {
                    thread::sleep(CONDITION_POLL);
                }
                Err(_) => return,
            }
        }
    });
    (
        address.port(),
        BackendThread {
            stop,
            wake_tcp: Some(address),
            wake_udp: None,
            handle: Some(handle),
        },
    )
}

fn spawn_udp_backend() -> (u16, BackendThread) {
    let socket = UdpSocket::bind((HOST, 0)).expect("bind UDP backend");
    socket
        .set_nonblocking(true)
        .expect("set UDP backend nonblocking");
    let address = socket.local_addr().expect("UDP backend address");
    let stop = Arc::new(AtomicBool::new(false));
    let thread_stop = Arc::clone(&stop);
    let handle = thread::spawn(move || {
        let mut bytes = [0u8; 2048];
        while !thread_stop.load(Ordering::Acquire) {
            match socket.recv_from(&mut bytes) {
                Ok((read, peer)) => {
                    let _ = socket.send_to(&bytes[..read], peer);
                }
                Err(error) if error.kind() == ErrorKind::WouldBlock => {
                    thread::sleep(CONDITION_POLL);
                }
                Err(_) => return,
            }
        }
    });
    (
        address.port(),
        BackendThread {
            stop,
            wake_tcp: None,
            wake_udp: Some(address),
            handle: Some(handle),
        },
    )
}

struct MainProcess {
    child: Child,
    process_group: u32,
    process_group_start_time: u64,
    stopped_worker: Option<(u32, u64)>,
}

fn process_parent_and_start_time(pid: u32) -> Result<(u32, u64), String> {
    let raw = std::fs::read_to_string(format!("/proc/{pid}/stat"))
        .map_err(|error| format!("read /proc/{pid}/stat: {error}"))?;
    let closing_parenthesis = raw
        .rfind(')')
        .ok_or_else(|| format!("parse /proc/{pid}/stat command"))?;
    let fields: Vec<&str> = raw[closing_parenthesis + 2..].split_whitespace().collect();
    let parent = fields
        .get(1)
        .ok_or_else(|| format!("missing PPID in /proc/{pid}/stat"))?
        .parse()
        .map_err(|error| format!("parse process {pid} PPID: {error}"))?;
    let start_time = fields
        .get(19)
        .ok_or_else(|| format!("missing start time in /proc/{pid}/stat"))?
        .parse()
        .map_err(|error| format!("parse process {pid} start time: {error}"))?;
    Ok((parent, start_time))
}

impl MainProcess {
    fn direct_child_start_time(&self, pid: u32) -> Result<u64, String> {
        let (_, main_start_time_before) = process_parent_and_start_time(self.process_group)?;
        if main_start_time_before != self.process_group_start_time {
            return Err(format!(
                "owned main PID {} was reused before signalling worker {pid}",
                self.process_group
            ));
        }
        let main_process_group = getpgid(Some(Pid::from_raw(self.process_group as i32)))
            .map_err(|error| format!("read owned main process group: {error}"))?;
        if main_process_group.as_raw() != self.process_group as i32 {
            return Err(format!(
                "owned main {} moved to process group {main_process_group}",
                self.process_group,
            ));
        }

        let (parent, start_time) = process_parent_and_start_time(pid)?;
        if parent != self.process_group {
            return Err(format!(
                "PPID {parent} is not owned main {}",
                self.process_group
            ));
        }
        let process_group = getpgid(Some(Pid::from_raw(pid as i32)))
            .map_err(|error| format!("read worker {pid} process group: {error}"))?;
        if process_group.as_raw() != self.process_group as i32 {
            return Err(format!(
                "worker {pid} process group {process_group} is not owned group {}",
                self.process_group,
            ));
        }

        let (_, main_start_time_after) = process_parent_and_start_time(self.process_group)?;
        if main_start_time_after != self.process_group_start_time {
            return Err(format!(
                "owned main PID {} changed while proving worker {pid}",
                self.process_group
            ));
        }
        Ok(start_time)
    }

    fn prove_direct_child(&self, pid: u32) -> u64 {
        self.direct_child_start_time(pid)
            .unwrap_or_else(|error| panic!("refusing to signal PID {pid}: {error}"))
    }

    fn stop_worker(&mut self, pid: u32) {
        let start_time = self.prove_direct_child(pid);
        kill(Pid::from_raw(pid as i32), Signal::SIGSTOP).expect("SIGSTOP proven owned worker");
        self.stopped_worker = Some((pid, start_time));
    }

    fn resume_worker(&mut self) {
        let Some((pid, start_time)) = self.stopped_worker.take() else {
            return;
        };
        let observed = self.prove_direct_child(pid);
        assert_eq!(
            observed, start_time,
            "refusing to resume a reused worker PID"
        );
        kill(Pid::from_raw(pid as i32), Signal::SIGCONT).expect("SIGCONT proven owned worker");
    }
}

impl Drop for MainProcess {
    fn drop(&mut self) {
        if let Some((pid, start_time)) = self.stopped_worker.take()
            && self.direct_child_start_time(pid).ok() == Some(start_time)
        {
            let _ = kill(Pid::from_raw(pid as i32), Signal::SIGCONT);
        }
        let _ = kill(Pid::from_raw(-(self.process_group as i32)), Signal::SIGTERM);
        let deadline = Instant::now() + Duration::from_secs(3);
        while Instant::now() < deadline {
            let _ = self.child.try_wait();
            if kill(Pid::from_raw(-(self.process_group as i32)), None).is_err() {
                return;
            }
            thread::sleep(CONDITION_POLL);
        }
        let _ = kill(Pid::from_raw(-(self.process_group as i32)), Signal::SIGKILL);
        let _ = self.child.wait();
    }
}

type Control = Channel<Request, Response>;

struct Instance {
    _temp: tempfile::TempDir,
    installed_binary: PathBuf,
    socket_path: PathBuf,
    pid_path: PathBuf,
    control: Control,
    main: MainProcess,
}

impl Instance {
    fn start(worker_count: u16) -> Self {
        Self::start_with_binary(worker_count, Path::new(sozu_bin()))
    }

    fn start_with_binary(worker_count: u16, source_binary: &Path) -> Self {
        let temp = tempfile::tempdir().expect("create mutable-command tempdir");
        let installed_binary = temp.path().join("sozu-installed");
        std::fs::copy(source_binary, &installed_binary).unwrap_or_else(|error| {
            panic!(
                "copy Sōzu binary {} into private fixture: {error}",
                source_binary.display()
            )
        });
        std::fs::set_permissions(&installed_binary, std::fs::Permissions::from_mode(0o755))
            .expect("make private Sōzu binary executable");
        let config_path = temp.path().join("runtime.toml");
        let socket_path = temp.path().join("sozu.sock");
        let pid_path = temp.path().join("sozu.pid");
        let log = File::create(temp.path().join("sozu.log")).expect("create Sōzu log");
        let log_err = log.try_clone().expect("clone Sōzu log");
        std::fs::write(
            &config_path,
            format!(
                r#"command_socket = "{}"
pid_file_path = "{}"
command_buffer_size = {COMMAND_BUFFER_SIZE}
max_command_buffer_size = {MAX_COMMAND_BUFFER_SIZE}
worker_count = {worker_count}
worker_automatic_restart = false
worker_timeout = 1
ctl_command_timeout = 15000
handle_process_affinity = false
log_level = "warn"
log_target = "stdout"
max_connections = 100
buffer_size = 16393
activate_listeners = false
"#,
                socket_path.display(),
                pid_path.display()
            ),
        )
        .expect("write runtime config");

        let mut command = observable_main_command(&installed_binary);
        command
            .args([
                "start",
                "-c",
                config_path.to_str().expect("UTF-8 config path"),
            ])
            .env_remove("NOTIFY_SOCKET")
            .stdout(Stdio::from(log))
            .stderr(Stdio::from(log_err))
            .process_group(0);
        let child = command.spawn().expect("spawn real Sōzu main");
        let process_group = child.id();
        let (_, process_group_start_time) = process_parent_and_start_time(process_group)
            .expect("record owned main process identity");
        let main = MainProcess {
            child,
            process_group,
            process_group_start_time,
            stopped_worker: None,
        };

        let mut control = wait_until("command socket", Duration::from_secs(10), || {
            if !socket_path.exists() {
                return None;
            }
            let mut channel = Channel::from_path(
                socket_path.to_str().expect("UTF-8 command socket"),
                COMMAND_BUFFER_SIZE,
                MAX_COMMAND_BUFFER_SIZE,
            )
            .ok()?;
            channel.blocking().ok()?;
            Some(channel)
        });
        let workers = wait_until("all workers running", Duration::from_secs(10), || {
            let response = send_raw(
                &mut control,
                RequestType::ListWorkers(ListWorkers {}),
                COMMAND_TIMEOUT,
            );
            running_worker_pids(&response)
                .filter(|workers| workers.len() == usize::from(worker_count))
        });
        assert_eq!(workers.len(), usize::from(worker_count));
        assert_eq!(
            workers.iter().copied().collect::<BTreeSet<_>>().len(),
            usize::from(worker_count),
            "the fixture must start distinct worker PIDs"
        );

        Self {
            _temp: temp,
            installed_binary,
            socket_path,
            pid_path,
            control,
            main,
        }
    }

    fn install_upgrade_binary(&self, source_binary: &Path) {
        let staged = self._temp.path().join("sozu-upgrade-staged");
        std::fs::copy(source_binary, &staged).unwrap_or_else(|error| {
            panic!(
                "stage Sōzu upgrade binary {} in private fixture: {error}",
                source_binary.display()
            )
        });
        std::fs::set_permissions(&staged, std::fs::Permissions::from_mode(0o755))
            .expect("make staged Sōzu upgrade binary executable");
        std::fs::rename(&staged, &self.installed_binary)
            .expect("atomically install private Sōzu upgrade binary");
    }

    fn load_state(&mut self, state: &ConfigState) {
        let path = self._temp.path().join("load.state");
        let mut file = File::create(&path).expect("create LoadState fixture");
        state
            .write_requests_to_file(&mut file)
            .expect("serialize LoadState fixture");
        drop(file);
        send_ok(
            self,
            RequestType::LoadState(path.to_string_lossy().into_owned()),
        );
    }

    fn main_pid(&self) -> u32 {
        std::fs::read_to_string(&self.pid_path)
            .expect("read main pid file")
            .trim()
            .parse()
            .expect("parse main pid")
    }

    fn reconnect_control(&mut self) {
        self.control = wait_until(
            "replacement main command socket",
            Duration::from_secs(10),
            || {
                let mut channel = Channel::from_path(
                    self.socket_path.to_str().expect("UTF-8 command socket"),
                    COMMAND_BUFFER_SIZE,
                    MAX_COMMAND_BUFFER_SIZE,
                )
                .ok()?;
                channel.blocking().ok()?;
                Some(channel)
            },
        );
    }

    fn assert_process_runs_installed_binary(&self, pid: u32, label: &str) {
        let process = std::fs::metadata(format!("/proc/{pid}/exe"))
            .unwrap_or_else(|error| panic!("read {label} executable identity: {error}"));
        let installed = std::fs::metadata(&self.installed_binary)
            .expect("read private installed executable identity");
        assert_eq!(
            (process.dev(), process.ino()),
            (installed.dev(), installed.ino()),
            "{label} PID {pid} does not execute the private installed candidate"
        );
    }

    fn upgrade_main(&mut self) {
        let old_main = self.main_pid();
        let response = send_raw(
            &mut self.control,
            RequestType::UpgradeMain(UpgradeMain {}),
            COMMAND_TIMEOUT,
        );
        assert_terminal_ok("UpgradeMain", &response);
        let new_main = wait_until("replacement main pid", Duration::from_secs(10), || {
            let new_main = self.main_pid();
            (new_main != old_main && std::path::Path::new(&format!("/proc/{new_main}")).exists())
                .then_some(new_main)
        });
        self.assert_process_runs_installed_binary(new_main, "replacement main");
        self.reconnect_control();
    }

    fn state_requests(&mut self) -> Vec<RequestType> {
        let path = self._temp.path().join("saved.state");
        let response = send_raw(
            &mut self.control,
            RequestType::SaveState(path.to_string_lossy().into_owned()),
            COMMAND_TIMEOUT,
        );
        assert_terminal_ok("SaveState", &response);
        let bytes = std::fs::read(path).expect("read saved state");
        let (remaining, requests) =
            parse_several_requests::<WorkerRequest>(&bytes).expect("parse production state file");
        assert!(remaining.is_empty(), "state parser left trailing bytes");
        requests
            .into_iter()
            .map(|request| request.content.request_type.expect("saved request type"))
            .collect()
    }
}

fn send_raw(control: &mut Control, request_type: RequestType, timeout: Duration) -> Response {
    let request: Request = request_type.into();
    control
        .write_message(&request)
        .expect("write command request");
    let deadline = Instant::now() + timeout;
    loop {
        let remaining = deadline.saturating_duration_since(Instant::now());
        assert!(
            !remaining.is_zero(),
            "command response exceeded {timeout:?}"
        );
        let response = control
            .read_message_blocking_timeout(Some(remaining))
            .expect("read command response");
        if response.status() != ResponseStatus::Processing {
            return response;
        }
    }
}

fn assert_terminal_ok(name: &str, response: &Response) {
    assert_eq!(
        response.status(),
        ResponseStatus::Ok,
        "{name} failed: {}",
        response.message
    );
}

fn running_worker_pids(response: &Response) -> Option<Vec<u32>> {
    Some(
        running_workers(response)?
            .into_iter()
            .map(|(_, pid)| pid)
            .collect(),
    )
}

fn running_workers(response: &Response) -> Option<Vec<(u32, u32)>> {
    let ContentType::Workers(workers) = response.content.as_ref()?.content_type.as_ref()? else {
        return None;
    };
    Some(
        workers
            .vec
            .iter()
            .filter(|worker| worker.run_state == 0 && worker.pid > 0)
            .map(|worker| (worker.id, worker.pid as u32))
            .collect(),
    )
}

fn worker_records(instance: &mut Instance) -> Vec<(u32, u32)> {
    let response = send_raw(
        &mut instance.control,
        RequestType::ListWorkers(ListWorkers {}),
        COMMAND_TIMEOUT,
    );
    assert_terminal_ok("ListWorkers", &response);
    running_workers(&response).expect("ListWorkers response content")
}

fn send_ok(instance: &mut Instance, request: RequestType) {
    let name = sozu_command_lib::proto::display::format_request_type(&request).to_owned();
    let response = send_raw(&mut instance.control, request, COMMAND_TIMEOUT);
    assert_terminal_ok(&name, &response);
}

fn http_frontend(listener: SocketAddress, cluster_id: &str) -> RequestHttpFrontend {
    RequestHttpFrontend {
        cluster_id: Some(cluster_id.to_owned()),
        address: listener,
        hostname: HTTP_HOST.to_owned(),
        path: PathRule {
            kind: PathRuleKind::Prefix as i32,
            value: "/".to_owned(),
        },
        position: RulePosition::Tree as i32,
        tags: BTreeMap::from([("test".to_owned(), "mutable-runtime".to_owned())]),
        ..Default::default()
    }
}

fn http_response(address: SocketAddr, host: &str) -> Option<String> {
    let mut stream = TcpStream::connect_timeout(&address, Duration::from_millis(250)).ok()?;
    stream.set_read_timeout(Some(Duration::from_secs(2))).ok()?;
    stream
        .write_all(
            format!("GET / HTTP/1.1\r\nHost: {host}\r\nConnection: close\r\n\r\n").as_bytes(),
        )
        .ok()?;
    let mut response = String::new();
    stream.read_to_string(&mut response).ok()?;
    Some(response)
}

fn http_body(address: SocketAddr, host: &str) -> Option<String> {
    let response = http_response(address, host)?;
    let (_, body) = response.split_once("\r\n\r\n")?;
    Some(body.to_owned())
}

fn https_body(address: SocketAddr, host: &str) -> Option<String> {
    let ca_certificate =
        PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../lib/assets/certificate.pem");
    let resolve = format!("{host}:{}:127.0.0.1", address.port());
    let url = format!("https://{host}:{}/", address.port());
    let output = Command::new("curl")
        .args([
            "--silent",
            "--show-error",
            "--fail",
            "--noproxy",
            "*",
            "--connect-timeout",
            "2",
            "--max-time",
            "4",
            "--cacert",
        ])
        .arg(ca_certificate)
        .args(["--resolve", &resolve, &url])
        .output()
        .ok()?;
    output
        .status
        .success()
        .then(|| String::from_utf8_lossy(&output.stdout).into_owned())
}

fn tls_peer_fingerprint(address: SocketAddr, host: &str) -> Option<String> {
    let connect = address.to_string();
    let mut child = Command::new("openssl")
        .args([
            "s_client",
            "-connect",
            &connect,
            "-servername",
            host,
            "-showcerts",
        ])
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()
        .ok()?;
    let mut stdout = match child.stdout.take() {
        Some(stdout) => stdout,
        None => {
            let _ = child.kill();
            let _ = child.wait();
            return None;
        }
    };
    let stdout_reader = thread::spawn(move || {
        let mut output = Vec::new();
        stdout.read_to_end(&mut output).map(|_| output)
    });
    let deadline = Instant::now() + TLS_PROBE_TIMEOUT;
    let status = loop {
        match child.try_wait() {
            Ok(Some(status)) => break Some(status),
            Ok(None) if Instant::now() < deadline => {
                thread::sleep(
                    CONDITION_POLL.min(deadline.saturating_duration_since(Instant::now())),
                );
            }
            Ok(None) | Err(_) => {
                let _ = child.kill();
                let _ = child.wait();
                break None;
            }
        }
    };
    let output = stdout_reader.join().ok()?.ok()?;
    let status = status?;
    if !status.success() {
        return None;
    }

    let stdout = String::from_utf8(output).ok()?;
    let begin = stdout.find("-----BEGIN CERTIFICATE-----")?;
    let end = stdout[begin..].find("-----END CERTIFICATE-----")?
        + begin
        + "-----END CERTIFICATE-----".len();
    CertificateAndKey {
        certificate: stdout[begin..end].to_owned(),
        ..Default::default()
    }
    .fingerprint()
    .ok()
    .map(|fingerprint| fingerprint.to_string())
}

#[test]
#[ignore = "local process-level: verifies the OpenSSL TLS probe deadline and child cleanup"]
fn tls_peer_fingerprint_times_out_and_reaps_a_stalled_openssl_child() {
    let listener = TcpListener::bind((HOST, 0)).expect("bind stalling TLS peer");
    let address = listener.local_addr().expect("stalling TLS peer address");
    let (accepted_tx, accepted_rx) = std::sync::mpsc::channel();
    let peer = thread::spawn(move || {
        let (mut stream, _) = listener.accept().expect("accept OpenSSL connection");
        accepted_tx
            .send(())
            .expect("report accepted OpenSSL connection");
        stream
            .set_read_timeout(Some(Duration::from_secs(6)))
            .expect("bound stalling TLS peer read");

        let mut buffer = [0; 1024];
        loop {
            match stream.read(&mut buffer) {
                Ok(0) => return true,
                Ok(_) => {}
                Err(error)
                    if matches!(error.kind(), ErrorKind::TimedOut | ErrorKind::WouldBlock) =>
                {
                    return false;
                }
                Err(error) => panic!("read stalling TLS peer: {error}"),
            }
        }
    });

    let started = Instant::now();
    assert_eq!(tls_peer_fingerprint(address, HTTPS_HOST), None);
    let elapsed = started.elapsed();
    accepted_rx
        .recv_timeout(Duration::from_secs(1))
        .expect("OpenSSL must connect to the stalling peer");
    assert!(
        elapsed <= Duration::from_secs(6),
        "the OpenSSL probe exceeded its bounded deadline: {elapsed:?}"
    );
    assert!(
        peer.join().expect("join stalling TLS peer"),
        "the exact OpenSSL child must close its connection before the probe returns"
    );
}

fn tcp_roundtrip(address: SocketAddr, payload: &[u8]) -> Option<Vec<u8>> {
    let mut stream = TcpStream::connect_timeout(&address, Duration::from_millis(250)).ok()?;
    stream.set_read_timeout(Some(Duration::from_secs(2))).ok()?;
    stream
        .set_write_timeout(Some(Duration::from_secs(2)))
        .ok()?;
    stream.write_all(payload).ok()?;
    let mut response = vec![0; payload.len()];
    stream.read_exact(&mut response).ok()?;
    Some(response)
}

fn udp_roundtrip(address: SocketAddr, payload: &[u8]) -> Option<Vec<u8>> {
    let socket = UdpSocket::bind((HOST, 0)).ok()?;
    socket.set_read_timeout(Some(Duration::from_secs(2))).ok()?;
    socket.send_to(payload, address).ok()?;
    let mut response = vec![0; payload.len() + 16];
    let (read, peer) = socket.recv_from(&mut response).ok()?;
    (peer == address).then_some(())?;
    response.truncate(read);
    Some(response)
}

fn assert_roundtrip(
    description: &str,
    expected: &[u8],
    mut exchange: impl FnMut() -> Option<Vec<u8>>,
) {
    wait_until(description, Duration::from_secs(10), || {
        (exchange().as_deref() == Some(expected)).then_some(())
    });
}

fn assert_http_backend(listener: SocketAddr, expected: &str, stage: &str) {
    let deadline = Instant::now() + Duration::from_secs(10);
    let mut last_response = None;
    while Instant::now() < deadline {
        last_response = http_response(listener, HTTP_HOST);
        if last_response
            .as_deref()
            .and_then(|response| response.split_once("\r\n\r\n"))
            .is_some_and(|(_, body)| body == expected)
        {
            return;
        }
        thread::sleep(CONDITION_POLL);
    }
    panic!(
        "{stage}: timed out waiting for {listener} to route host {HTTP_HOST} to backend body \
         {expected:?}; last response: {last_response:?}"
    );
}

fn assert_http_status_and_body(
    listener: SocketAddr,
    expected_status: &str,
    expected_body: &str,
    stage: &str,
) {
    let deadline = Instant::now() + Duration::from_secs(10);
    let mut last_response = None;
    while Instant::now() < deadline {
        last_response = http_response(listener, HTTP_HOST);
        if last_response.as_deref().is_some_and(|response| {
            response.starts_with(expected_status)
                && response
                    .split_once("\r\n\r\n")
                    .is_some_and(|(_, body)| body == expected_body)
        }) {
            return;
        }
        thread::sleep(CONDITION_POLL);
    }
    panic!(
        "{stage}: timed out waiting for {listener} to return {expected_status:?} with body \
         {expected_body:?}; last response: {last_response:?}"
    );
}

fn saved_backends(instance: &mut Instance, cluster_id: &str) -> BTreeMap<String, SocketAddress> {
    instance
        .state_requests()
        .into_iter()
        .filter_map(|request| match request {
            RequestType::AddBackend(backend) if backend.cluster_id == cluster_id => {
                Some((backend.backend_id, backend.address))
            }
            _ => None,
        })
        .collect()
}

fn saved_http_listener(
    instance: &mut Instance,
    address: SocketAddress,
) -> Option<HttpListenerConfig> {
    instance
        .state_requests()
        .into_iter()
        .find_map(|request| match request {
            RequestType::AddHttpListener(listener) if listener.address == address => Some(listener),
            _ => None,
        })
}

fn saved_has_http_frontend(instance: &mut Instance, expected: &RequestHttpFrontend) -> bool {
    instance.state_requests().into_iter().any(
        |request| matches!(request, RequestType::AddHttpFrontend(frontend) if frontend == *expected),
    )
}

fn saved_has_https_frontend(instance: &mut Instance, expected: &RequestHttpFrontend) -> bool {
    instance.state_requests().into_iter().any(
        |request| matches!(request, RequestType::AddHttpsFrontend(frontend) if frontend == *expected),
    )
}

fn saved_has_tcp_frontend(instance: &mut Instance, expected: &RequestTcpFrontend) -> bool {
    instance.state_requests().into_iter().any(
        |request| matches!(request, RequestType::AddTcpFrontend(frontend) if frontend == *expected),
    )
}

fn saved_has_udp_frontend(instance: &mut Instance, expected: &RequestUdpFrontend) -> bool {
    instance.state_requests().into_iter().any(
        |request| matches!(request, RequestType::AddUdpFrontend(frontend) if frontend == *expected),
    )
}

fn saved_has_certificate(instance: &mut Instance, expected: &AddCertificate) -> bool {
    instance.state_requests().into_iter().any(
        |request| matches!(request, RequestType::AddCertificate(certificate) if certificate.address == expected.address && certificate.certificate == expected.certificate),
    )
}

fn saved_certificate_order_and_expirations(
    instance: &mut Instance,
    expected_address: SocketAddress,
) -> Vec<(String, Option<i64>)> {
    instance
        .state_requests()
        .into_iter()
        .filter_map(|request| match request {
            RequestType::AddCertificate(certificate) if certificate.address == expected_address => {
                Some((
                    certificate
                        .certificate
                        .fingerprint()
                        .expect("fingerprint saved certificate")
                        .to_string(),
                    certificate.expired_at,
                ))
            }
            _ => None,
        })
        .collect()
}

fn saved_certificates(instance: &mut Instance) -> BTreeMap<String, CertificateAndKey> {
    instance
        .state_requests()
        .into_iter()
        .filter_map(|request| match request {
            RequestType::AddCertificate(certificate) => Some((
                certificate
                    .certificate
                    .fingerprint()
                    .expect("fingerprint saved certificate")
                    .to_string(),
                certificate.certificate,
            )),
            _ => None,
        })
        .collect()
}

fn main_certificates(instance: &mut Instance) -> BTreeMap<String, CertificateAndKey> {
    let response = send_raw(
        &mut instance.control,
        RequestType::QueryCertificatesFromTheState(QueryCertificatesFilters {
            domain: None,
            fingerprint: None,
        }),
        COMMAND_TIMEOUT,
    );
    assert_terminal_ok("QueryCertificatesFromTheState", &response);
    let Some(ContentType::CertificatesWithFingerprints(certificates)) = response
        .content
        .as_ref()
        .and_then(|content| content.content_type.as_ref())
    else {
        panic!("QueryCertificatesFromTheState returned an unexpected payload");
    };
    certificates.certs.clone()
}

fn main_serving_certificate_fingerprint(instance: &mut Instance, domain: &str) -> String {
    let response = send_raw(
        &mut instance.control,
        RequestType::QueryCertificatesFromTheState(QueryCertificatesFilters {
            domain: Some(domain.to_owned()),
            fingerprint: None,
        }),
        COMMAND_TIMEOUT,
    );
    assert_terminal_ok("QueryCertificatesFromTheState(domain)", &response);
    let Some(ContentType::CertificatesWithFingerprints(certificates)) = response
        .content
        .as_ref()
        .and_then(|content| content.content_type.as_ref())
    else {
        panic!("domain state certificate query returned an unexpected payload");
    };
    assert_eq!(
        certificates.certs.len(),
        1,
        "main state must resolve exactly one certificate for {domain}"
    );
    certificates
        .certs
        .keys()
        .next()
        .expect("one main certificate fingerprint")
        .to_owned()
}

fn worker_certificate_summaries(
    instance: &mut Instance,
    expected_address: SocketAddress,
) -> BTreeMap<String, Vec<(String, String)>> {
    let expected_workers: BTreeSet<String> = worker_records(instance)
        .into_iter()
        .map(|(worker_id, _)| worker_id.to_string())
        .collect();
    let response = send_raw(
        &mut instance.control,
        RequestType::QueryCertificatesFromWorkers(QueryCertificatesFilters {
            domain: None,
            fingerprint: None,
        }),
        COMMAND_TIMEOUT,
    );
    assert_terminal_ok("QueryCertificatesFromWorkers", &response);
    let Some(ContentType::WorkerResponses(processes)) = response
        .content
        .as_ref()
        .and_then(|content| content.content_type.as_ref())
    else {
        panic!("QueryCertificatesFromWorkers returned an unexpected payload");
    };
    assert_eq!(
        processes.map.keys().cloned().collect::<BTreeSet<_>>(),
        expected_workers,
        "certificate query must return every currently running worker"
    );

    processes
        .map
        .iter()
        .map(|(process, content)| {
            let Some(ContentType::CertificatesByAddress(addresses)) = content.content_type.as_ref()
            else {
                panic!("{process} returned a non-certificate payload");
            };
            let mut summaries: Vec<_> = addresses
                .certificates
                .iter()
                .filter(|certificates| certificates.address == expected_address)
                .flat_map(|certificates| &certificates.certificate_summaries)
                .map(|summary| (summary.domain.clone(), summary.fingerprint.clone()))
                .collect();
            summaries.sort();
            (process.clone(), summaries)
        })
        .collect()
}

fn assert_every_worker_has_certificate(
    instance: &mut Instance,
    expected: &AddCertificate,
) -> BTreeMap<String, Vec<(String, String)>> {
    let fingerprint = expected
        .certificate
        .fingerprint()
        .expect("fingerprint expected certificate")
        .to_string();
    let expected_summary = (HTTPS_HOST.to_owned(), fingerprint);
    let summaries = worker_certificate_summaries(instance, expected.address);
    assert!(!summaries.is_empty(), "certificate query omitted workers");
    for (worker, certificates) in &summaries {
        assert!(
            certificates.contains(&expected_summary),
            "worker {worker} resolver omitted expected domain and fingerprint: {certificates:?}"
        );
    }
    summaries
}

fn worker_serving_certificate_fingerprints(
    instance: &mut Instance,
    expected_address: SocketAddress,
    domain: &str,
) -> BTreeMap<String, String> {
    let expected_workers: BTreeSet<String> = worker_records(instance)
        .into_iter()
        .map(|(worker_id, _)| worker_id.to_string())
        .collect();
    let response = send_raw(
        &mut instance.control,
        RequestType::QueryCertificatesFromWorkers(QueryCertificatesFilters {
            domain: Some(domain.to_owned()),
            fingerprint: None,
        }),
        COMMAND_TIMEOUT,
    );
    assert_terminal_ok("QueryCertificatesFromWorkers(domain)", &response);
    let Some(ContentType::WorkerResponses(processes)) = response
        .content
        .as_ref()
        .and_then(|content| content.content_type.as_ref())
    else {
        panic!("domain certificate query returned an unexpected payload");
    };
    assert_eq!(
        processes.map.keys().cloned().collect::<BTreeSet<_>>(),
        expected_workers,
        "domain certificate query must return every currently running worker"
    );

    processes
        .map
        .iter()
        .map(|(worker, content)| {
            let Some(ContentType::CertificatesByAddress(addresses)) = content.content_type.as_ref()
            else {
                panic!("{worker} returned a non-certificate payload");
            };
            let summaries: Vec<_> = addresses
                .certificates
                .iter()
                .filter(|certificates| certificates.address == expected_address)
                .flat_map(|certificates| &certificates.certificate_summaries)
                .collect();
            assert_eq!(
                summaries.len(),
                1,
                "{worker} must resolve exactly one certificate for {domain}"
            );
            (worker.to_owned(), summaries[0].fingerprint.to_owned())
        })
        .collect()
}

fn worker_min_certificate_expirations(instance: &mut Instance) -> BTreeMap<String, u64> {
    let expected_workers: BTreeSet<String> = worker_records(instance)
        .into_iter()
        .map(|(worker_id, _)| worker_id.to_string())
        .collect();
    let response = send_raw(
        &mut instance.control,
        RequestType::QueryMetrics(QueryMetricsOptions {
            list: false,
            cluster_ids: Vec::new(),
            backend_ids: Vec::new(),
            metric_names: vec![TLS_MIN_EXPIRY_METRIC.to_owned()],
            no_clusters: true,
            workers: true,
        }),
        COMMAND_TIMEOUT,
    );
    assert_terminal_ok("QueryMetrics(tls cert expiration)", &response);
    let Some(ContentType::Metrics(metrics)) = response
        .content
        .as_ref()
        .and_then(|content| content.content_type.as_ref())
    else {
        panic!("TLS certificate expiration query returned an unexpected payload");
    };
    assert_eq!(
        metrics.workers.keys().cloned().collect::<BTreeSet<_>>(),
        expected_workers,
        "metrics query must return every currently running worker"
    );
    metrics
        .workers
        .iter()
        .map(|(worker, metrics)| {
            let metric = metrics
                .proxy
                .get(TLS_MIN_EXPIRY_METRIC)
                .unwrap_or_else(|| panic!("worker {worker} omitted {TLS_MIN_EXPIRY_METRIC}"));
            let Some(Inner::Gauge(expiration)) = metric.inner else {
                panic!("worker {worker} returned a non-gauge TLS certificate expiration metric");
            };
            (worker.to_owned(), expiration)
        })
        .collect()
}

fn assert_failure(name: &str, response: &Response) {
    assert_eq!(
        response.status(),
        ResponseStatus::Failure,
        "{name} unexpectedly succeeded: {}",
        response.message
    );
}

fn start_serving_https(
    backend_label: &'static str,
    cluster_id: &str,
) -> (
    Instance,
    BackendThread,
    SocketAddr,
    SocketAddress,
    AddCertificate,
) {
    let (backend_port, backend) = spawn_http_backend(backend_label);
    let listener_address = SocketAddr::from(([127, 0, 0, 1], free_tcp_port()));
    let listener: SocketAddress = listener_address.into();
    let mut frontend = http_frontend(listener, cluster_id);
    frontend.hostname = HTTPS_HOST.to_owned();
    let certificate = AddCertificate {
        address: listener,
        certificate: CertificateAndKey {
            certificate: include_str!("../../lib/assets/certificate.pem").to_owned(),
            certificate_chain: Vec::new(),
            key: include_str!("../../lib/assets/key.pem").to_owned(),
            versions: Vec::new(),
            names: vec![HTTPS_HOST.to_owned()],
        },
        expired_at: None,
    };
    let mut instance = Instance::start(1);
    send_ok(
        &mut instance,
        RequestType::AddCluster(Cluster {
            cluster_id: cluster_id.to_owned(),
            ..Default::default()
        }),
    );
    send_ok(
        &mut instance,
        RequestType::AddHttpsListener(
            ListenerBuilder::new_https(listener)
                .to_tls(None)
                .expect("build HTTPS listener config"),
        ),
    );
    send_ok(&mut instance, RequestType::AddHttpsFrontend(frontend));
    send_ok(
        &mut instance,
        RequestType::AddBackend(AddBackend {
            cluster_id: cluster_id.to_owned(),
            backend_id: backend_label.to_owned(),
            address: SocketAddr::from(([127, 0, 0, 1], backend_port)).into(),
            ..Default::default()
        }),
    );
    send_ok(
        &mut instance,
        RequestType::AddCertificate(certificate.clone()),
    );
    send_ok(
        &mut instance,
        RequestType::ActivateListener(ActivateListener {
            address: listener,
            proxy: ListenerType::Https as i32,
            from_scm: false,
            interface: None,
        }),
    );
    wait_until("initial HTTPS route", Duration::from_secs(10), || {
        (https_body(listener_address, HTTPS_HOST).as_deref()
            == Some(format!("{backend_label}\n").as_str()))
        .then_some(())
    });
    (instance, backend, listener_address, listener, certificate)
}

fn certificate_with_invalid_private_key() -> CertificateAndKey {
    CertificateAndKey {
        certificate: include_str!("../../lib/assets/tests/certificate-1y.pem").to_owned(),
        certificate_chain: Vec::new(),
        key: "not a PEM-encoded private key".to_owned(),
        versions: Vec::new(),
        names: vec![HTTPS_HOST.to_owned()],
    }
}

fn certificate_with_invalid_name() -> CertificateAndKey {
    CertificateAndKey {
        certificate: include_str!("../../lib/assets/tests/certificate-1y.pem").to_owned(),
        certificate_chain: Vec::new(),
        key: include_str!("../../lib/assets/tests/key-1y.pem").to_owned(),
        versions: Vec::new(),
        names: vec!["/te.*/.example.com".to_owned()],
    }
}

fn certificate_with_mismatched_private_key() -> CertificateAndKey {
    CertificateAndKey {
        certificate: include_str!("../../lib/assets/tests/certificate-1y.pem").to_owned(),
        certificate_chain: Vec::new(),
        key: include_str!("../../lib/assets/tests/key-2y.pem").to_owned(),
        versions: Vec::new(),
        names: vec![HTTPS_HOST.to_owned()],
    }
}

fn assert_rejected_certificate_mutation_preserves_tls_and_state(
    backend_label: &'static str,
    request: impl FnOnce(SocketAddress, &AddCertificate) -> RequestType,
) {
    let (mut instance, _backend, listener_address, listener, certificate) =
        start_serving_https(backend_label, backend_label);
    let saved_before = saved_certificates(&mut instance);
    let main_before = main_certificates(&mut instance);
    let workers_before = assert_every_worker_has_certificate(&mut instance, &certificate);
    let expected_fingerprint = certificate
        .certificate
        .fingerprint()
        .expect("fingerprint original certificate")
        .to_string();
    assert!(
        saved_before.contains_key(&expected_fingerprint),
        "saved state omitted the original certificate before the refusal"
    );
    assert!(
        main_before.contains_key(&expected_fingerprint),
        "main certificate state omitted the original certificate before the refusal"
    );

    let response = send_raw(
        &mut instance.control,
        request(listener, &certificate),
        COMMAND_TIMEOUT,
    );
    assert_failure(backend_label, &response);

    assert_eq!(saved_certificates(&mut instance), saved_before);
    assert_eq!(main_certificates(&mut instance), main_before);
    assert_eq!(
        assert_every_worker_has_certificate(&mut instance, &certificate),
        workers_before
    );
    let expected_body = format!("{backend_label}\n");
    wait_until(
        "original certificate and route after rejected certificate mutation",
        Duration::from_secs(10),
        || {
            (https_body(listener_address, HTTPS_HOST).as_deref() == Some(expected_body.as_str()))
                .then_some(())
        },
    );
}

fn cluster_snapshots(
    instance: &mut Instance,
    cluster_id: &str,
) -> BTreeMap<String, ClusterInformation> {
    let mut expected_processes: BTreeSet<String> = worker_records(instance)
        .into_iter()
        .map(|(worker_id, _)| worker_id.to_string())
        .collect();
    expected_processes.insert("main".to_owned());
    let response = send_raw(
        &mut instance.control,
        RequestType::QueryClusterById(cluster_id.to_owned()),
        COMMAND_TIMEOUT,
    );
    assert_terminal_ok("QueryClusterById", &response);
    let Some(ContentType::WorkerResponses(processes)) = response
        .content
        .as_ref()
        .and_then(|content| content.content_type.as_ref())
    else {
        panic!("QueryClusterById returned no per-process cluster state");
    };
    assert_eq!(
        processes.map.keys().cloned().collect::<BTreeSet<_>>(),
        expected_processes,
        "QueryClusterById must return main and every currently running worker"
    );

    processes
        .map
        .iter()
        .map(|(process, content)| {
            let Some(ContentType::Clusters(clusters)) = content.content_type.as_ref() else {
                panic!("{process} returned a non-cluster QueryClusterById payload");
            };
            assert_eq!(
                clusters.vec.len(),
                1,
                "{process} returned an unexpected cluster count"
            );
            let cluster = clusters.vec[0].clone();
            assert_eq!(
                cluster
                    .configuration
                    .as_ref()
                    .map(|configuration| configuration.cluster_id.as_str()),
                Some(cluster_id),
                "{process} returned the wrong cluster"
            );
            (process.to_owned(), cluster)
        })
        .collect()
}

fn assert_every_process_keeps_http_frontend(
    instance: &mut Instance,
    cluster_id: &str,
    expected: &RequestHttpFrontend,
) {
    for (process, cluster) in cluster_snapshots(instance, cluster_id) {
        assert!(
            cluster.http_frontends.contains(expected),
            "{process} lost the retained HTTP frontend"
        );
    }
}

fn start_http_without_frontend(
    worker_count: u16,
    cluster_id: &str,
    backend_label: &'static str,
) -> (Instance, BackendThread, SocketAddr, RequestHttpFrontend) {
    let (backend_port, backend) = spawn_http_backend(backend_label);
    let listener_address = SocketAddr::from(([127, 0, 0, 1], free_tcp_port()));
    let listener: SocketAddress = listener_address.into();
    let frontend = http_frontend(listener, cluster_id);
    let mut instance = Instance::start(worker_count);

    send_ok(
        &mut instance,
        RequestType::AddCluster(Cluster {
            cluster_id: cluster_id.to_owned(),
            ..Default::default()
        }),
    );
    send_ok(
        &mut instance,
        RequestType::AddHttpListener(
            ListenerBuilder::new_http(listener)
                .to_http(None)
                .expect("build HTTP listener config"),
        ),
    );
    send_ok(
        &mut instance,
        RequestType::AddBackend(AddBackend {
            cluster_id: cluster_id.to_owned(),
            backend_id: backend_label.to_owned(),
            address: SocketAddr::from(([127, 0, 0, 1], backend_port)).into(),
            ..Default::default()
        }),
    );
    send_ok(
        &mut instance,
        RequestType::ActivateListener(ActivateListener {
            address: listener,
            proxy: ListenerType::Http as i32,
            from_scm: false,
            interface: None,
        }),
    );
    wait_until("initial route absence", Duration::from_secs(10), || {
        http_response(listener_address, HTTP_HOST)
            .is_some_and(|response| response.starts_with("HTTP/1.1 404"))
            .then_some(())
    });
    assert!(!saved_has_http_frontend(&mut instance, &frontend));

    (instance, backend, listener_address, frontend)
}

#[test]
#[ignore = "process-level: stops one of two owned workers to exercise a mixed acknowledgement timeout"]
fn mixed_ack_timeout_converges_when_the_stopped_worker_processes_its_buffered_order() {
    let cluster_id = "mixed-ack-timeout";
    let (mut instance, _backend, listener_address, frontend) =
        start_http_without_frontend(2, cluster_id, "mixed-ack");
    let workers = worker_records(&mut instance);
    assert_eq!(workers.len(), 2, "the test needs exactly two workers");
    let (_, stopped_pid) = workers[0];
    instance.main.stop_worker(stopped_pid);

    let response = send_raw(
        &mut instance.control,
        RequestType::AddHttpFrontend(frontend.to_owned()),
        COMMAND_TIMEOUT,
    );
    assert_failure("AddHttpFrontend with one stopped worker", &response);
    assert!(
        saved_has_http_frontend(&mut instance, &frontend),
        "one worker acknowledgement must keep the desired frontend in main state"
    );

    instance.main.resume_worker();
    let snapshots = cluster_snapshots(&mut instance, cluster_id);
    for (process, cluster) in snapshots {
        assert!(
            cluster.http_frontends.contains(&frontend),
            "{process} did not converge after the stopped worker processed its buffered order"
        );
    }
    assert_eq!(
        http_body(listener_address, HTTP_HOST).as_deref(),
        Some("mixed-ack\n"),
        "real traffic must use the frontend after every worker converged"
    );
}

#[test]
#[ignore = "process-level: stops one of two owned workers past the server worker deadline to prove QueryClusterById never reports a partial fleet as Success"]
fn query_cluster_by_id_fails_when_a_stopped_worker_misses_the_deadline_then_recovers() {
    let cluster_id = "query-timeout-partial";
    let mut instance = Instance::start(2);
    send_ok(
        &mut instance,
        RequestType::AddCluster(Cluster {
            cluster_id: cluster_id.to_owned(),
            ..Default::default()
        }),
    );
    let initial = cluster_snapshots(&mut instance, cluster_id);
    assert_eq!(
        initial.keys().map(String::as_str).collect::<Vec<_>>(),
        ["0", "1", "main"],
        "both workers and main must be queryable before the stop"
    );

    let workers = worker_records(&mut instance);
    assert_eq!(workers.len(), 2, "the test needs exactly two workers");
    let (_, stopped_pid) = workers[0];
    instance.main.stop_worker(stopped_pid);

    // COMMAND_TIMEOUT (15 s) exceeds the fixture's `worker_timeout = 1`, so the
    // server deadline fires first and the client still receives a terminal reply.
    let response = send_raw(
        &mut instance.control,
        RequestType::QueryClusterById(cluster_id.to_owned()),
        COMMAND_TIMEOUT,
    );
    assert_failure("QueryClusterById with one stopped worker", &response);
    assert!(
        response.content.is_none(),
        "a failed QueryClusterById must not return a partial payload"
    );

    instance.main.resume_worker();
    let recovered = cluster_snapshots(&mut instance, cluster_id);
    assert_eq!(
        recovered.keys().map(String::as_str).collect::<Vec<_>>(),
        ["0", "1", "main"],
        "after SIGCONT the same query must succeed with main and both workers"
    );
}

#[test]
#[ignore = "process-level: stops the only owned worker, observes a late success, then reconciles with UpgradeWorker"]
fn zero_ack_timeout_rolls_back_main_state_and_worker_upgrade_reconciles_a_late_success() {
    let cluster_id = "zero-ack-timeout";
    let (mut instance, _backend, listener_address, frontend) =
        start_http_without_frontend(1, cluster_id, "zero-ack");
    let workers = worker_records(&mut instance);
    assert_eq!(workers.len(), 1, "the test needs exactly one worker");
    let (worker_id, stopped_pid) = workers[0];
    instance.main.stop_worker(stopped_pid);

    let response = send_raw(
        &mut instance.control,
        RequestType::AddHttpFrontend(frontend.to_owned()),
        COMMAND_TIMEOUT,
    );
    assert_failure("AddHttpFrontend with no acknowledgement", &response);
    assert!(
        !saved_has_http_frontend(&mut instance, &frontend),
        "zero acknowledgements must roll the desired frontend out of main state"
    );

    instance.main.resume_worker();
    let divergent = cluster_snapshots(&mut instance, cluster_id);
    assert!(
        !divergent["main"].http_frontends.contains(&frontend),
        "main must retain the rolled-back state"
    );
    assert!(
        divergent
            .iter()
            .filter(|(process, _)| process.as_str() != "main")
            .all(|(_, cluster)| cluster.http_frontends.contains(&frontend)),
        "the resumed worker must expose the accepted late-success residual"
    );
    assert_eq!(
        http_body(listener_address, HTTP_HOST).as_deref(),
        Some("zero-ack\n"),
        "the late worker success must be observable in real routing before recovery"
    );

    let response = send_raw(
        &mut instance.control,
        RequestType::UpgradeWorker(worker_id),
        COMMAND_TIMEOUT,
    );
    assert_terminal_ok("UpgradeWorker recovery", &response);
    let replacement = worker_records(&mut instance);
    assert_eq!(
        replacement.len(),
        1,
        "upgrade must leave one running worker"
    );
    assert_ne!(
        replacement[0].1, stopped_pid,
        "recovery must replace the divergent worker process"
    );

    let reconciled = cluster_snapshots(&mut instance, cluster_id);
    for (process, cluster) in reconciled {
        assert!(
            !cluster.http_frontends.contains(&frontend),
            "{process} retained the late frontend after worker replay recovery"
        );
    }
    wait_until(
        "recovered worker to serve the authoritative 404",
        Duration::from_secs(10),
        || {
            http_response(listener_address, HTTP_HOST)
                .is_some_and(|response| response.starts_with("HTTP/1.1 404"))
                .then_some(())
        },
    );
    assert!(!saved_has_http_frontend(&mut instance, &frontend));
}

#[test]
#[ignore = "process-level: sends a patch rejected by the production listener-template validator"]
fn rejected_active_http_listener_update_preserves_state_and_routing() {
    let cluster_id = "rejected-listener-update";
    let (mut instance, _backend, listener_address, frontend) =
        start_http_without_frontend(1, cluster_id, "listener-update");
    send_ok(
        &mut instance,
        RequestType::AddHttpFrontend(frontend.to_owned()),
    );
    assert_http_backend(
        listener_address,
        "listener-update\n",
        "before the rejected listener update",
    );
    let listener: SocketAddress = listener_address.into();
    let saved_before = saved_http_listener(&mut instance, listener)
        .expect("active HTTP listener in saved state before patch");

    let response = send_raw(
        &mut instance.control,
        RequestType::UpdateHttpListener(UpdateHttpListenerConfig {
            address: listener,
            front_timeout: Some(saved_before.front_timeout + 1),
            answers: BTreeMap::from([(
                "503".to_owned(),
                "not a valid HTTP response template".to_owned(),
            )]),
            ..Default::default()
        }),
        COMMAND_TIMEOUT,
    );
    assert_failure("invalid UpdateHttpListener", &response);
    assert_eq!(
        saved_http_listener(&mut instance, listener).as_ref(),
        Some(&saved_before),
        "a rejected active-listener patch must not change saved state"
    );
    assert_http_backend(
        listener_address,
        "listener-update\n",
        "after the rejected listener update",
    );
}

#[test]
#[ignore = "process-level: exercises an AddCluster upsert rejected by the live answer registry"]
fn rejected_cluster_upsert_with_invalid_runtime_answers_preserves_state_and_routing() {
    let cluster_id = "rejected-cluster-upsert";
    let (mut instance, _backend, listener_address, frontend) =
        start_http_without_frontend(1, cluster_id, "cluster-upsert");
    send_ok(
        &mut instance,
        RequestType::AddHttpFrontend(frontend.to_owned()),
    );
    assert_http_backend(
        listener_address,
        "cluster-upsert\n",
        "before the rejected cluster upsert",
    );
    let before = cluster_snapshots(&mut instance, cluster_id);

    let response = send_raw(
        &mut instance.control,
        RequestType::AddCluster(Cluster {
            cluster_id: cluster_id.to_owned(),
            sticky_session: true,
            answers: BTreeMap::from([(
                "503".to_owned(),
                "not a valid HTTP response template".to_owned(),
            )]),
            ..Default::default()
        }),
        COMMAND_TIMEOUT,
    );
    assert_failure("invalid AddCluster upsert", &response);
    assert_eq!(
        cluster_snapshots(&mut instance, cluster_id),
        before,
        "a cluster upsert refused by the runtime must preserve every process state"
    );
    assert_http_backend(
        listener_address,
        "cluster-upsert\n",
        "after the rejected cluster upsert",
    );
}

#[test]
#[ignore = "process-level: proves a refused cluster upsert keeps the live cluster 503 answer"]
fn rejected_cluster_upsert_preserves_existing_503_answer() {
    let cluster_id = "rejected-cluster-answer-upsert";
    let backend_id = "cluster-answer-upsert";
    let (mut instance, _backend, listener_address, frontend) =
        start_http_without_frontend(1, cluster_id, backend_id);
    send_ok(
        &mut instance,
        RequestType::AddHttpFrontend(frontend.to_owned()),
    );

    let original_body = "original cluster answer";
    let original_answer = format!(
        "HTTP/1.1 503 Service Unavailable\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{original_body}",
        original_body.len()
    );
    send_ok(
        &mut instance,
        RequestType::AddCluster(Cluster {
            cluster_id: cluster_id.to_owned(),
            answer_503: Some(original_answer),
            ..Default::default()
        }),
    );
    let backend_address = *saved_backends(&mut instance, cluster_id)
        .get(backend_id)
        .expect("saved backend address");
    send_ok(
        &mut instance,
        RequestType::RemoveBackend(RemoveBackend {
            cluster_id: cluster_id.to_owned(),
            backend_id: backend_id.to_owned(),
            address: backend_address,
        }),
    );
    assert_http_status_and_body(
        listener_address,
        "HTTP/1.1 503",
        original_body,
        "before the rejected cluster-answer upsert",
    );
    let before = cluster_snapshots(&mut instance, cluster_id);

    let response = send_raw(
        &mut instance.control,
        RequestType::AddCluster(Cluster {
            cluster_id: cluster_id.to_owned(),
            sticky_session: true,
            answers: BTreeMap::from([(
                "503".to_owned(),
                "not a valid HTTP response template".to_owned(),
            )]),
            ..Default::default()
        }),
        COMMAND_TIMEOUT,
    );
    assert_failure("invalid AddCluster answer upsert", &response);
    assert_eq!(
        cluster_snapshots(&mut instance, cluster_id),
        before,
        "a refused answer upsert must preserve every process cluster state"
    );
    assert_http_status_and_body(
        listener_address,
        "HTTP/1.1 503",
        original_body,
        "after the rejected cluster-answer upsert",
    );
}

#[test]
#[ignore = "process-level: spawns a real main and worker; run from the dedicated CI step or with --ignored (see module docs)"]
fn backend_mutations_change_saved_state_and_real_http_routing() {
    let (backend_a_port, _backend_a) = spawn_http_backend("backend-a");
    let (backend_b_port, _backend_b) = spawn_http_backend("backend-b");
    let listener_address = SocketAddr::from(([127, 0, 0, 1], free_tcp_port()));
    let listener: SocketAddress = listener_address.into();
    let backend_a: SocketAddress = SocketAddr::from(([127, 0, 0, 1], backend_a_port)).into();
    let backend_b: SocketAddress = SocketAddr::from(([127, 0, 0, 1], backend_b_port)).into();
    let cluster_id = "mutable-http";
    let mut instance = Instance::start(1);

    send_ok(
        &mut instance,
        RequestType::AddCluster(Cluster {
            cluster_id: cluster_id.to_owned(),
            ..Default::default()
        }),
    );
    send_ok(
        &mut instance,
        RequestType::AddHttpListener(HttpListenerConfig {
            address: listener,
            sticky_name: "SOZUBALANCEID".to_owned(),
            front_timeout: 60,
            back_timeout: 30,
            connect_timeout: 3,
            request_timeout: 10,
            ..Default::default()
        }),
    );
    send_ok(
        &mut instance,
        RequestType::AddHttpFrontend(http_frontend(listener, cluster_id)),
    );
    let add_a = AddBackend {
        cluster_id: cluster_id.to_owned(),
        backend_id: "backend-a".to_owned(),
        address: backend_a,
        ..Default::default()
    };
    send_ok(&mut instance, RequestType::AddBackend(add_a.clone()));
    send_ok(
        &mut instance,
        RequestType::ActivateListener(ActivateListener {
            address: listener,
            proxy: ListenerType::Http as i32,
            from_scm: false,
            interface: None,
        }),
    );
    assert_http_backend(listener_address, "backend-a\n", "initial backend A");
    assert_eq!(
        saved_backends(&mut instance, cluster_id),
        BTreeMap::from([("backend-a".to_owned(), backend_a)])
    );

    let add_b = AddBackend {
        cluster_id: cluster_id.to_owned(),
        backend_id: "backend-b".to_owned(),
        address: backend_b,
        ..Default::default()
    };
    send_ok(&mut instance, RequestType::AddBackend(add_b.clone()));
    send_ok(
        &mut instance,
        RequestType::RemoveBackend(RemoveBackend {
            cluster_id: cluster_id.to_owned(),
            backend_id: add_a.backend_id.clone(),
            address: add_a.address,
        }),
    );
    assert_http_backend(listener_address, "backend-b\n", "after replacing A with B");
    assert_eq!(
        saved_backends(&mut instance, cluster_id),
        BTreeMap::from([("backend-b".to_owned(), backend_b)])
    );

    send_ok(&mut instance, RequestType::AddBackend(add_a));
    send_ok(
        &mut instance,
        RequestType::RemoveBackend(RemoveBackend {
            cluster_id: cluster_id.to_owned(),
            backend_id: add_b.backend_id,
            address: add_b.address,
        }),
    );
    assert_http_backend(listener_address, "backend-a\n", "after restoring backend A");
    assert_eq!(
        saved_backends(&mut instance, cluster_id),
        BTreeMap::from([("backend-a".to_owned(), backend_a)])
    );
}

#[test]
#[ignore = "process-level: spawns a real main and worker; run from the dedicated CI step or with --ignored (see module docs)"]
fn http_frontend_and_listener_mutations_change_state_and_socket_behavior() {
    let (backend_port, _backend) = spawn_http_backend("frontend-listener");
    let listener_address = SocketAddr::from(([127, 0, 0, 1], free_tcp_port()));
    let listener: SocketAddress = listener_address.into();
    let cluster_id = "mutable-http-listener";
    let frontend = http_frontend(listener, cluster_id);
    let listener_config = HttpListenerConfig {
        address: listener,
        sticky_name: "SOZUBALANCEID".to_owned(),
        front_timeout: 60,
        back_timeout: 30,
        connect_timeout: 3,
        request_timeout: 10,
        ..Default::default()
    };
    let mut instance = Instance::start(1);

    send_ok(
        &mut instance,
        RequestType::AddCluster(Cluster {
            cluster_id: cluster_id.to_owned(),
            ..Default::default()
        }),
    );
    send_ok(
        &mut instance,
        RequestType::AddHttpListener(listener_config.clone()),
    );
    send_ok(
        &mut instance,
        RequestType::AddHttpFrontend(frontend.clone()),
    );
    send_ok(
        &mut instance,
        RequestType::AddBackend(AddBackend {
            cluster_id: cluster_id.to_owned(),
            backend_id: "frontend-listener".to_owned(),
            address: SocketAddr::from(([127, 0, 0, 1], backend_port)).into(),
            ..Default::default()
        }),
    );
    send_ok(
        &mut instance,
        RequestType::ActivateListener(ActivateListener {
            address: listener,
            proxy: ListenerType::Http as i32,
            from_scm: false,
            interface: None,
        }),
    );
    assert_http_backend(
        listener_address,
        "frontend-listener\n",
        "initial HTTP frontend and listener",
    );
    assert!(saved_has_http_frontend(&mut instance, &frontend));

    send_ok(
        &mut instance,
        RequestType::RemoveHttpFrontend(frontend.clone()),
    );
    wait_until(
        "removed HTTP frontend to answer 404",
        Duration::from_secs(10),
        || {
            http_response(listener_address, HTTP_HOST)
                .is_some_and(|response| response.starts_with("HTTP/1.1 404"))
                .then_some(())
        },
    );
    assert!(!saved_has_http_frontend(&mut instance, &frontend));
    send_ok(
        &mut instance,
        RequestType::AddHttpFrontend(frontend.clone()),
    );
    assert_http_backend(
        listener_address,
        "frontend-listener\n",
        "after re-adding the HTTP frontend",
    );

    send_ok(
        &mut instance,
        RequestType::UpdateHttpListener(UpdateHttpListenerConfig {
            address: listener,
            front_timeout: Some(61),
            ..Default::default()
        }),
    );
    assert_eq!(
        saved_http_listener(&mut instance, listener)
            .expect("updated HTTP listener in saved state")
            .front_timeout,
        61
    );
    assert_http_backend(
        listener_address,
        "frontend-listener\n",
        "after updating the HTTP listener",
    );

    send_ok(
        &mut instance,
        RequestType::DeactivateListener(DeactivateListener {
            address: listener,
            proxy: ListenerType::Http as i32,
            to_scm: false,
            interface: None,
        }),
    );
    wait_until(
        "HTTP listener socket to close",
        Duration::from_secs(10),
        || {
            TcpStream::connect_timeout(&listener_address, Duration::from_millis(100))
                .is_err()
                .then_some(())
        },
    );
    assert!(
        !saved_http_listener(&mut instance, listener)
            .expect("deactivated HTTP listener in saved state")
            .active
    );

    send_ok(
        &mut instance,
        RequestType::ActivateListener(ActivateListener {
            address: listener,
            proxy: ListenerType::Http as i32,
            from_scm: false,
            interface: None,
        }),
    );
    assert_http_backend(
        listener_address,
        "frontend-listener\n",
        "after reactivating the HTTP listener",
    );
    assert!(
        saved_http_listener(&mut instance, listener)
            .expect("reactivated HTTP listener in saved state")
            .active
    );

    send_ok(
        &mut instance,
        RequestType::RemoveListener(RemoveListener {
            address: listener,
            proxy: ListenerType::Http as i32,
            interface: None,
        }),
    );
    wait_until(
        "removed HTTP listener socket to close",
        Duration::from_secs(10),
        || {
            TcpStream::connect_timeout(&listener_address, Duration::from_millis(100))
                .is_err()
                .then_some(())
        },
    );
    assert!(saved_http_listener(&mut instance, listener).is_none());
    assert!(saved_has_http_frontend(&mut instance, &frontend));
    assert_every_process_keeps_http_frontend(&mut instance, cluster_id, &frontend);

    send_ok(&mut instance, RequestType::AddHttpListener(listener_config));
    assert!(saved_has_http_frontend(&mut instance, &frontend));
    assert_every_process_keeps_http_frontend(&mut instance, cluster_id, &frontend);
    send_ok(
        &mut instance,
        RequestType::ActivateListener(ActivateListener {
            address: listener,
            proxy: ListenerType::Http as i32,
            from_scm: false,
            interface: None,
        }),
    );
    assert_http_backend(
        listener_address,
        "frontend-listener\n",
        "after removing and re-adding the HTTP listener",
    );
}

#[test]
#[ignore = "process-level: occupies the listener port to force a real activation failure"]
fn rejected_activate_listener_preserves_inactive_state_and_can_retry() {
    let listener_address = SocketAddr::from(([127, 0, 0, 1], free_tcp_port()));
    let listener: SocketAddress = listener_address.into();
    let listener_config = ListenerBuilder::new_http(listener)
        .to_http(None)
        .expect("build HTTP listener config");
    let mut instance = Instance::start(1);
    send_ok(&mut instance, RequestType::AddHttpListener(listener_config));
    let occupied = TcpListener::bind(listener_address).expect("occupy listener port");

    let response = send_raw(
        &mut instance.control,
        RequestType::ActivateListener(ActivateListener {
            address: listener,
            proxy: ListenerType::Http as i32,
            from_scm: false,
            interface: None,
        }),
        COMMAND_TIMEOUT,
    );
    assert_failure("ActivateListener on an occupied port", &response);
    assert_eq!(
        saved_http_listener(&mut instance, listener).map(|config| config.active),
        Some(false),
        "a refused activation must keep the persisted listener inactive"
    );

    drop(occupied);
    assert!(
        TcpStream::connect_timeout(&listener_address, Duration::from_millis(250)).is_err(),
        "the refused activation must not leave a worker socket bound"
    );
    send_ok(
        &mut instance,
        RequestType::ActivateListener(ActivateListener {
            address: listener,
            proxy: ListenerType::Http as i32,
            from_scm: false,
            interface: None,
        }),
    );
    wait_until(
        "retry activation to bind the released port",
        Duration::from_secs(10),
        || {
            TcpStream::connect_timeout(&listener_address, Duration::from_millis(250))
                .is_ok()
                .then_some(())
        },
    );
}

#[test]
#[ignore = "process-level: spawns a real main and worker and invokes curl for a verified TLS handshake"]
fn https_listener_readdition_restores_frontend_certificate_and_routing() {
    let (backend_port, _backend) = spawn_http_backend("https-listener");
    let listener_address = SocketAddr::from(([127, 0, 0, 1], free_tcp_port()));
    let listener: SocketAddress = listener_address.into();
    let cluster_id = "mutable-https-listener";
    let mut frontend = http_frontend(listener, cluster_id);
    frontend.hostname = HTTPS_HOST.to_owned();
    let listener_config = ListenerBuilder::new_https(listener)
        .to_tls(None)
        .expect("build HTTPS listener config");
    let certificate = AddCertificate {
        address: listener,
        certificate: CertificateAndKey {
            certificate: include_str!("../../lib/assets/certificate.pem").to_owned(),
            certificate_chain: Vec::new(),
            key: include_str!("../../lib/assets/key.pem").to_owned(),
            versions: Vec::new(),
            names: vec![HTTPS_HOST.to_owned()],
        },
        expired_at: None,
    };
    let mut instance = Instance::start(1);

    send_ok(
        &mut instance,
        RequestType::AddCluster(Cluster {
            cluster_id: cluster_id.to_owned(),
            ..Default::default()
        }),
    );
    send_ok(
        &mut instance,
        RequestType::AddHttpsListener(listener_config.clone()),
    );
    send_ok(
        &mut instance,
        RequestType::AddHttpsFrontend(frontend.clone()),
    );
    send_ok(
        &mut instance,
        RequestType::AddBackend(AddBackend {
            cluster_id: cluster_id.to_owned(),
            backend_id: "https-listener".to_owned(),
            address: SocketAddr::from(([127, 0, 0, 1], backend_port)).into(),
            ..Default::default()
        }),
    );
    send_ok(
        &mut instance,
        RequestType::AddCertificate(certificate.clone()),
    );
    send_ok(
        &mut instance,
        RequestType::ActivateListener(ActivateListener {
            address: listener,
            proxy: ListenerType::Https as i32,
            from_scm: false,
            interface: None,
        }),
    );
    wait_until("initial HTTPS route", Duration::from_secs(10), || {
        (https_body(listener_address, HTTPS_HOST).as_deref() == Some("https-listener\n"))
            .then_some(())
    });
    let certificates_before = assert_every_worker_has_certificate(&mut instance, &certificate);

    send_ok(
        &mut instance,
        RequestType::RemoveListener(RemoveListener {
            address: listener,
            proxy: ListenerType::Https as i32,
            interface: None,
        }),
    );
    assert!(saved_has_https_frontend(&mut instance, &frontend));
    assert!(saved_has_certificate(&mut instance, &certificate));
    for (process, cluster) in cluster_snapshots(&mut instance, cluster_id) {
        assert!(
            cluster.https_frontends.contains(&frontend),
            "{process} lost the retained HTTPS frontend"
        );
    }

    send_ok(
        &mut instance,
        RequestType::AddHttpsListener(listener_config),
    );
    send_ok(
        &mut instance,
        RequestType::ActivateListener(ActivateListener {
            address: listener,
            proxy: ListenerType::Https as i32,
            from_scm: false,
            interface: None,
        }),
    );
    wait_until(
        "HTTPS route and certificate after listener re-addition",
        Duration::from_secs(10),
        || {
            (https_body(listener_address, HTTPS_HOST).as_deref() == Some("https-listener\n"))
                .then_some(())
        },
    );
    assert_eq!(
        assert_every_worker_has_certificate(&mut instance, &certificate),
        certificates_before,
        "listener re-addition must restore the exact worker certificate summaries"
    );
}

/// Regression coverage for the historical loss of effective `expired_at`
/// values when the last HTTPS listener was removed. The two static
/// certificates share one key and are both overridden to serve `HTTPS_HOST`;
/// their X.509 expiry order is B > A, while the explicit runtime order below
/// is A > B. SaveState, listener reconstruction, main upgrade and worker
/// upgrade must all preserve the selected fingerprint and minimum-expiry
/// gauge.
#[test]
#[ignore = "process-level: compares live TLS, resolver and expiry gauge across HTTPS listener re-addition"]
fn https_listener_readdition_preserves_effective_certificate_expirations() {
    let listener_address = SocketAddr::from(([127, 0, 0, 1], free_tcp_port()));
    let listener: SocketAddress = listener_address.into();
    let listener_config = ListenerBuilder::new_https(listener)
        .to_tls(None)
        .expect("build HTTPS listener config");
    let certificate_a = AddCertificate {
        address: listener,
        certificate: CertificateAndKey {
            certificate: include_str!("../../lib/assets/tests/certificate-1.pem").to_owned(),
            key: include_str!("../../lib/assets/tests/key.pem").to_owned(),
            names: vec![HTTPS_HOST.to_owned()],
            ..Default::default()
        },
        expired_at: Some(4_000_000_000),
    };
    let certificate_b = AddCertificate {
        address: listener,
        certificate: CertificateAndKey {
            certificate: include_str!("../../lib/assets/tests/certificate-2.pem").to_owned(),
            key: include_str!("../../lib/assets/tests/key.pem").to_owned(),
            names: vec![HTTPS_HOST.to_owned()],
            ..Default::default()
        },
        expired_at: Some(3_000_000_000),
    };
    let fingerprint_a = certificate_a
        .certificate
        .fingerprint()
        .expect("fingerprint certificate A")
        .to_string();
    let fingerprint_b = certificate_b
        .certificate
        .fingerprint()
        .expect("fingerprint certificate B")
        .to_string();
    assert_ne!(fingerprint_a, fingerprint_b);

    let mut instance = Instance::start(1);
    send_ok(
        &mut instance,
        RequestType::AddHttpsListener(listener_config.to_owned()),
    );
    send_ok(
        &mut instance,
        RequestType::AddCertificate(certificate_a.to_owned()),
    );
    send_ok(
        &mut instance,
        RequestType::AddCertificate(certificate_b.to_owned()),
    );
    send_ok(
        &mut instance,
        RequestType::ActivateListener(ActivateListener {
            address: listener,
            proxy: ListenerType::Https as i32,
            from_scm: false,
            interface: None,
        }),
    );

    let resolver_before =
        worker_serving_certificate_fingerprints(&mut instance, listener, HTTPS_HOST);
    assert!(
        resolver_before
            .values()
            .all(|fingerprint| fingerprint == &fingerprint_a),
        "the explicit expiration must make certificate A the resolver choice: {resolver_before:?}"
    );
    let tls_before = wait_until(
        "certificate A in the real TLS handshake",
        Duration::from_secs(10),
        || {
            tls_peer_fingerprint(listener_address, HTTPS_HOST)
                .filter(|fingerprint| fingerprint == &fingerprint_a)
        },
    );
    let gauge_before = worker_min_certificate_expirations(&mut instance);
    assert!(
        gauge_before
            .values()
            .all(|expiration| *expiration == 3_000_000_000),
        "the minimum expiration must be certificate B's explicit override: {gauge_before:?}"
    );
    let before = (resolver_before, tls_before.to_owned(), gauge_before);
    let expected_saved = vec![
        (fingerprint_a.to_owned(), Some(4_000_000_000)),
        (fingerprint_b.to_owned(), Some(3_000_000_000)),
    ];
    assert_eq!(
        saved_certificate_order_and_expirations(&mut instance, listener),
        expected_saved,
        "SaveState must retain certificate insertion order and expiration overrides"
    );

    send_ok(
        &mut instance,
        RequestType::RemoveListener(RemoveListener {
            address: listener,
            proxy: ListenerType::Https as i32,
            interface: None,
        }),
    );
    assert!(saved_has_certificate(&mut instance, &certificate_a));
    assert!(saved_has_certificate(&mut instance, &certificate_b));
    send_ok(
        &mut instance,
        RequestType::AddHttpsListener(listener_config),
    );
    send_ok(
        &mut instance,
        RequestType::ActivateListener(ActivateListener {
            address: listener,
            proxy: ListenerType::Https as i32,
            from_scm: false,
            interface: None,
        }),
    );

    let after = (
        worker_serving_certificate_fingerprints(&mut instance, listener, HTTPS_HOST),
        wait_until(
            "a certificate in the real TLS handshake after listener re-addition",
            Duration::from_secs(10),
            || tls_peer_fingerprint(listener_address, HTTPS_HOST),
        ),
        worker_min_certificate_expirations(&mut instance),
    );
    assert_eq!(
        after, before,
        "listener re-addition must preserve resolver choice, real TLS fingerprint and the worker minimum-expiry gauge; certificate B is {fingerprint_b}"
    );
    assert_eq!(
        saved_certificate_order_and_expirations(&mut instance, listener),
        expected_saved,
        "listener reconstruction must not change the persisted certificate metadata"
    );

    instance.upgrade_main();
    assert_eq!(
        saved_certificate_order_and_expirations(&mut instance, listener),
        expected_saved,
        "main upgrade must retain certificate insertion order and expiration overrides"
    );

    let workers = worker_records(&mut instance);
    assert_eq!(workers.len(), 1, "the test needs exactly one worker");
    let (worker_id, old_worker_pid) = workers[0];
    send_ok(&mut instance, RequestType::UpgradeWorker(worker_id));
    wait_until(
        "replacement certificate worker",
        Duration::from_secs(10),
        || {
            let workers = worker_records(&mut instance);
            (workers.len() == 1 && workers[0].1 != old_worker_pid).then_some(())
        },
    );
    let resolver_after_upgrades =
        worker_serving_certificate_fingerprints(&mut instance, listener, HTTPS_HOST);
    assert!(
        resolver_after_upgrades
            .values()
            .all(|fingerprint| fingerprint == &fingerprint_a),
        "the replacement worker must keep certificate A as the resolver choice: {resolver_after_upgrades:?}"
    );
    let tls_after_upgrades = wait_until(
        "certificate A in the real TLS handshake after main and worker upgrades",
        Duration::from_secs(10),
        || tls_peer_fingerprint(listener_address, HTTPS_HOST),
    );
    assert_eq!(
        tls_after_upgrades, tls_before,
        "main and worker upgrades must preserve the certificate presented in the real TLS handshake"
    );
    let gauge_after_upgrades = worker_min_certificate_expirations(&mut instance);
    assert!(
        gauge_after_upgrades
            .values()
            .all(|expiration| *expiration == 3_000_000_000),
        "the replacement worker must keep certificate B's explicit minimum expiration: {gauge_after_upgrades:?}"
    );
    assert_eq!(
        saved_certificate_order_and_expirations(&mut instance, listener),
        expected_saved,
        "worker bootstrap must leave the authoritative SaveState metadata unchanged"
    );
}

/// Characterize the first upgrade from the published 2.3.1 state shape,
/// which had no persistent certificate expiration or insertion-order
/// metadata. The legacy worker receives explicit overrides from a state file,
/// but its main process saves only certificate material. After UpgradeMain,
/// the candidate must accept that legacy snapshot without inventing the lost
/// values. Until UpgradeWorker, the candidate main and legacy worker can
/// therefore select different certificates. Replacing the worker converges
/// both sides on the candidate's deterministic legacy fallback; every later
/// candidate SaveState/replay and main/worker upgrade must remain stable.
#[test]
#[ignore = "external compatibility boundary: requires SOZU_CERTIFICATE_METADATA_LEGACY=published-2.3.1-binary"]
fn published_2_3_1_certificate_metadata_loss_is_bounded_to_the_first_upgrade() {
    let Some(legacy_binary) = std::env::var_os("SOZU_CERTIFICATE_METADATA_LEGACY")
        .filter(|path| !path.is_empty())
        .map(PathBuf::from)
    else {
        let mut stderr = std::io::stderr().lock();
        stderr
            .write_all(
                b"\nskipping published_2_3_1_certificate_metadata_loss_is_bounded_to_the_first_upgrade: set SOZU_CERTIFICATE_METADATA_LEGACY to the frozen 2.3.1 binary\n",
            )
            .expect("write visible external-artifact skip");
        stderr
            .flush()
            .expect("flush visible external-artifact skip");
        return;
    };
    assert!(legacy_binary.is_file(), "legacy 2.3.1 binary is missing");
    let legacy_version = Command::new(&legacy_binary)
        .arg("--version")
        .output()
        .expect("query legacy binary version");
    let legacy_version = String::from_utf8_lossy(&legacy_version.stdout);
    assert!(
        legacy_version.contains("sozu 2.3.1 (1cc9bf2"),
        "legacy binary must be the published 1cc9bf2 2.3.1 release, got {legacy_version:?}"
    );

    let listener_address = SocketAddr::from(([127, 0, 0, 1], free_tcp_port()));
    let listener: SocketAddress = listener_address.into();
    let certificate_a = AddCertificate {
        address: listener,
        certificate: CertificateAndKey {
            certificate: include_str!("../../lib/assets/tests/certificate-1.pem").to_owned(),
            key: include_str!("../../lib/assets/tests/key.pem").to_owned(),
            names: vec![HTTPS_HOST.to_owned()],
            ..Default::default()
        },
        expired_at: Some(4_000_000_000),
    };
    let certificate_b = AddCertificate {
        address: listener,
        certificate: CertificateAndKey {
            certificate: include_str!("../../lib/assets/tests/certificate-2.pem").to_owned(),
            key: include_str!("../../lib/assets/tests/key.pem").to_owned(),
            names: vec![HTTPS_HOST.to_owned()],
            ..Default::default()
        },
        expired_at: Some(3_000_000_000),
    };
    let fingerprint_a = certificate_a
        .certificate
        .fingerprint()
        .expect("fingerprint certificate A")
        .to_string();
    let fingerprint_b = certificate_b
        .certificate
        .fingerprint()
        .expect("fingerprint certificate B")
        .to_string();

    let mut legacy_state = ConfigState::new();
    for request_type in [
        RequestType::AddHttpsListener(
            ListenerBuilder::new_https(listener)
                .to_tls(None)
                .expect("build HTTPS listener config"),
        ),
        RequestType::AddCertificate(certificate_a),
        RequestType::AddCertificate(certificate_b),
        RequestType::ActivateListener(ActivateListener {
            address: listener,
            proxy: ListenerType::Https as i32,
            from_scm: false,
            interface: None,
        }),
    ] {
        legacy_state
            .dispatch(&request_type.into())
            .expect("build certificate compatibility state");
    }

    let mut instance = Instance::start_with_binary(1, &legacy_binary);
    instance.load_state(&legacy_state);
    let legacy_worker =
        worker_serving_certificate_fingerprints(&mut instance, listener, HTTPS_HOST);
    assert!(
        legacy_worker
            .values()
            .all(|fingerprint| fingerprint == &fingerprint_a),
        "2.3.1 worker must initially honor A's larger explicit override: {legacy_worker:?}"
    );
    assert_eq!(
        wait_until(
            "certificate A from the legacy worker",
            Duration::from_secs(10),
            || tls_peer_fingerprint(listener_address, HTTPS_HOST),
        ),
        fingerprint_a
    );
    let legacy_gauge = worker_min_certificate_expirations(&mut instance);
    assert!(
        legacy_gauge
            .values()
            .all(|expiration| *expiration == 3_000_000_000),
        "2.3.1 worker must expose B's explicit minimum expiration: {legacy_gauge:?}"
    );

    let legacy_saved = saved_certificate_order_and_expirations(&mut instance, listener);
    assert_eq!(
        legacy_saved.len(),
        2,
        "legacy SaveState must keep both certificates"
    );
    assert!(
        legacy_saved
            .iter()
            .all(|(_, expired_at)| expired_at.is_none()),
        "2.3.1 SaveState must demonstrate the historical expiration loss: {legacy_saved:?}"
    );
    assert_eq!(
        legacy_saved
            .iter()
            .map(|(fingerprint, _)| fingerprint)
            .collect::<BTreeSet<_>>(),
        BTreeSet::from([&fingerprint_a, &fingerprint_b]),
        "legacy SaveState must still carry both certificate fingerprints"
    );

    instance.install_upgrade_binary(Path::new(sozu_bin()));
    instance.upgrade_main();
    let candidate_main_winner = main_serving_certificate_fingerprint(&mut instance, HTTPS_HOST);
    assert_eq!(
        candidate_main_winner, fingerprint_b,
        "without historical overrides, the candidate main must select B by X.509 expiration"
    );
    let mixed_generation_worker =
        worker_serving_certificate_fingerprints(&mut instance, listener, HTTPS_HOST);
    assert!(
        mixed_generation_worker
            .values()
            .all(|fingerprint| fingerprint == &fingerprint_a),
        "the still-running 2.3.1 worker must retain its historical resolver until replacement: {mixed_generation_worker:?}"
    );
    assert_eq!(
        tls_peer_fingerprint(listener_address, HTTPS_HOST).as_deref(),
        Some(fingerprint_a.as_str()),
        "the mixed-generation interval must still serve from the legacy worker"
    );

    let workers = worker_records(&mut instance);
    assert_eq!(
        workers.len(),
        1,
        "the compatibility fixture needs one worker"
    );
    let (legacy_worker_id, legacy_worker_pid) = workers[0];
    send_ok(&mut instance, RequestType::UpgradeWorker(legacy_worker_id));
    let (_, candidate_worker_pid) = wait_until(
        "candidate replacement worker",
        Duration::from_secs(10),
        || {
            let workers = worker_records(&mut instance);
            (workers.len() == 1 && workers[0].1 != legacy_worker_pid).then_some(workers[0])
        },
    );
    instance.assert_process_runs_installed_binary(candidate_worker_pid, "replacement worker");
    let candidate_worker =
        worker_serving_certificate_fingerprints(&mut instance, listener, HTTPS_HOST);
    assert!(
        candidate_worker
            .values()
            .all(|fingerprint| fingerprint == &fingerprint_b),
        "the candidate worker must converge to the candidate main's legacy fallback: {candidate_worker:?}"
    );
    assert_eq!(
        wait_until(
            "certificate B after first candidate worker replacement",
            Duration::from_secs(10),
            || tls_peer_fingerprint(listener_address, HTTPS_HOST),
        ),
        fingerprint_b
    );
    let candidate_gauge = worker_min_certificate_expirations(&mut instance);
    assert!(
        candidate_gauge
            .values()
            .all(|expiration| *expiration != 3_000_000_000 && *expiration > 0),
        "the candidate worker must expose a real X.509 expiry rather than 2.3.1's lost explicit override: {candidate_gauge:?}"
    );
    let candidate_saved = saved_certificate_order_and_expirations(&mut instance, listener);
    assert_eq!(candidate_saved.len(), 2);
    assert!(
        candidate_saved
            .iter()
            .all(|(_, expired_at)| expired_at.is_none()),
        "the candidate must not invent expiration overrides missing from the legacy snapshot: {candidate_saved:?}"
    );

    instance.upgrade_main();
    assert_eq!(
        main_serving_certificate_fingerprint(&mut instance, HTTPS_HOST),
        fingerprint_b,
        "a candidate-to-candidate main upgrade must preserve the normalized winner"
    );
    let workers = worker_records(&mut instance);
    assert_eq!(workers.len(), 1);
    let (candidate_worker_id, candidate_worker_pid) = workers[0];
    send_ok(
        &mut instance,
        RequestType::UpgradeWorker(candidate_worker_id),
    );
    let (_, second_candidate_worker_pid) = wait_until(
        "second candidate replacement worker",
        Duration::from_secs(10),
        || {
            let workers = worker_records(&mut instance);
            (workers.len() == 1 && workers[0].1 != candidate_worker_pid).then_some(workers[0])
        },
    );
    instance.assert_process_runs_installed_binary(
        second_candidate_worker_pid,
        "second replacement worker",
    );
    assert_eq!(
        saved_certificate_order_and_expirations(&mut instance, listener),
        candidate_saved,
        "the first candidate SaveState must be stable across later candidate upgrades"
    );
    assert!(
        worker_serving_certificate_fingerprints(&mut instance, listener, HTTPS_HOST)
            .values()
            .all(|fingerprint| fingerprint == &fingerprint_b)
    );
    assert_eq!(
        tls_peer_fingerprint(listener_address, HTTPS_HOST).as_deref(),
        Some(fingerprint_b.as_str())
    );
    let gauge_after_second_worker = worker_min_certificate_expirations(&mut instance);
    assert_eq!(
        gauge_after_second_worker
            .values()
            .copied()
            .collect::<Vec<_>>(),
        candidate_gauge.values().copied().collect::<Vec<_>>(),
        "candidate-to-candidate worker replacement must preserve the normalized expiry gauge value despite the expected worker-id change"
    );
}

/// The minimal A,B -> B,A order-only diff is Remove(A), Add(A). The
/// intermediate state must keep B serving, and replaying the second request
/// must make A the equal-expiration winner in both the worker query and the
/// certificate actually presented on the TLS connection. This test observes
/// the presented certificate; the separately verified HTTPS listener test
/// covers CA and HTTP routing validation with the repository's trusted
/// certificate.
#[test]
#[ignore = "process-level: observes the TLS winner before, during and after a minimal certificate-order recovery"]
fn certificate_order_recovery_moves_only_the_required_runtime_entry() {
    let listener_address = SocketAddr::from(([127, 0, 0, 1], free_tcp_port()));
    let listener: SocketAddress = listener_address.into();
    let listener_config = ListenerBuilder::new_https(listener)
        .to_tls(None)
        .expect("build HTTPS listener config");
    let certificate_a = AddCertificate {
        address: listener,
        certificate: CertificateAndKey {
            certificate: include_str!("../../lib/assets/tests/certificate-1.pem").to_owned(),
            key: include_str!("../../lib/assets/tests/key.pem").to_owned(),
            names: vec![HTTPS_HOST.to_owned()],
            ..Default::default()
        },
        expired_at: Some(3_000_000_000),
    };
    let certificate_b = AddCertificate {
        address: listener,
        certificate: CertificateAndKey {
            certificate: include_str!("../../lib/assets/tests/certificate-2.pem").to_owned(),
            key: include_str!("../../lib/assets/tests/key.pem").to_owned(),
            names: vec![HTTPS_HOST.to_owned()],
            ..Default::default()
        },
        expired_at: Some(3_000_000_000),
    };
    let fingerprint_a = certificate_a
        .certificate
        .fingerprint()
        .expect("fingerprint A")
        .to_string();
    let fingerprint_b = certificate_b
        .certificate
        .fingerprint()
        .expect("fingerprint B")
        .to_string();
    let mut instance = Instance::start(1);
    send_ok(
        &mut instance,
        RequestType::AddHttpsListener(listener_config),
    );
    send_ok(
        &mut instance,
        RequestType::AddCertificate(certificate_a.to_owned()),
    );
    send_ok(
        &mut instance,
        RequestType::AddCertificate(certificate_b.to_owned()),
    );
    send_ok(
        &mut instance,
        RequestType::ActivateListener(ActivateListener {
            address: listener,
            proxy: ListenerType::Https as i32,
            from_scm: false,
            interface: None,
        }),
    );

    let initial = worker_serving_certificate_fingerprints(&mut instance, listener, HTTPS_HOST);
    assert!(
        initial
            .values()
            .all(|fingerprint| fingerprint == &fingerprint_b),
        "the later equal-expiration certificate B must win initially: {initial:?}"
    );
    assert_eq!(
        wait_until(
            "certificate B in the initial TLS handshake",
            Duration::from_secs(10),
            || tls_peer_fingerprint(listener_address, HTTPS_HOST),
        ),
        fingerprint_b
    );

    send_ok(
        &mut instance,
        RequestType::RemoveCertificate(RemoveCertificate {
            address: listener,
            fingerprint: fingerprint_a.to_owned(),
        }),
    );
    let interrupted = worker_serving_certificate_fingerprints(&mut instance, listener, HTTPS_HOST);
    assert!(
        interrupted
            .values()
            .all(|fingerprint| fingerprint == &fingerprint_b),
        "removing A must leave retained certificate B serving: {interrupted:?}"
    );
    assert_eq!(
        tls_peer_fingerprint(listener_address, HTTPS_HOST).as_deref(),
        Some(fingerprint_b.as_str()),
        "the partial sequence must keep certificate B presented on the live TLS socket"
    );

    send_ok(&mut instance, RequestType::AddCertificate(certificate_a));
    let recovered = worker_serving_certificate_fingerprints(&mut instance, listener, HTTPS_HOST);
    assert!(
        recovered
            .values()
            .all(|fingerprint| fingerprint == &fingerprint_a),
        "re-adding A at the end must make it the equal-expiration winner: {recovered:?}"
    );
    assert_eq!(
        wait_until(
            "certificate A in the recovered TLS handshake",
            Duration::from_secs(10),
            || tls_peer_fingerprint(listener_address, HTTPS_HOST),
        ),
        fingerprint_a
    );
    assert_eq!(
        saved_certificate_order_and_expirations(&mut instance, listener),
        vec![
            (fingerprint_b, Some(3_000_000_000)),
            (fingerprint_a, Some(3_000_000_000)),
        ],
        "SaveState must record the recovered B,A order exactly"
    );
}

/// Replacing a certificate with the same PEM fingerprint but different
/// effective names and expiration must update the runtime resolver as well as
/// ConfigState. This guards the historical fingerprint-only replacement
/// shortcut where main stored the new payload while the worker kept serving
/// the old names and expiration.
#[test]
#[ignore = "process-level: compares state, resolver, real TLS and expiry after a same-fingerprint replacement"]
fn same_fingerprint_certificate_replacement_updates_runtime_payload() {
    const REPLACEMENT_HOST: &str = "replacement.lolcatho.st";

    let listener_address = SocketAddr::from(([127, 0, 0, 1], free_tcp_port()));
    let listener: SocketAddress = listener_address.into();
    let initial_certificate = AddCertificate {
        address: listener,
        certificate: CertificateAndKey {
            certificate: include_str!("../../lib/assets/tests/certificate-1y.pem").to_owned(),
            key: include_str!("../../lib/assets/tests/key-1y.pem").to_owned(),
            names: vec![HTTPS_HOST.to_owned()],
            ..Default::default()
        },
        expired_at: Some(4_000_000_000),
    };
    let fingerprint = initial_certificate
        .certificate
        .fingerprint()
        .expect("fingerprint initial certificate")
        .to_string();
    let mut replacement_certificate = initial_certificate.certificate.to_owned();
    replacement_certificate.names = vec![REPLACEMENT_HOST.to_owned()];

    let mut instance = Instance::start(1);
    send_ok(
        &mut instance,
        RequestType::AddHttpsListener(
            ListenerBuilder::new_https(listener)
                .to_tls(None)
                .expect("build HTTPS listener config"),
        ),
    );
    send_ok(
        &mut instance,
        RequestType::AddCertificate(initial_certificate.to_owned()),
    );
    send_ok(
        &mut instance,
        RequestType::ActivateListener(ActivateListener {
            address: listener,
            proxy: ListenerType::Https as i32,
            from_scm: false,
            interface: None,
        }),
    );
    assert!(
        worker_certificate_summaries(&mut instance, listener)
            .values()
            .all(|summaries| summaries == &vec![(HTTPS_HOST.to_owned(), fingerprint.to_owned())]),
        "the initial resolver must expose the original name and fingerprint"
    );

    send_ok(
        &mut instance,
        RequestType::ReplaceCertificate(ReplaceCertificate {
            address: listener,
            new_certificate: replacement_certificate.to_owned(),
            old_fingerprint: fingerprint.to_owned(),
            new_expired_at: Some(2_000_000_000),
        }),
    );

    let saved_after = saved_certificates(&mut instance);
    let main_after = main_certificates(&mut instance);
    assert_eq!(
        saved_after.get(&fingerprint),
        Some(&replacement_certificate),
        "SaveState must expose the replacement payload"
    );
    assert_eq!(
        main_after.get(&fingerprint),
        Some(&replacement_certificate),
        "main state must expose the replacement payload"
    );

    let worker_after = worker_certificate_summaries(&mut instance, listener);
    let expected_worker_after: BTreeMap<_, _> = worker_after
        .keys()
        .map(|worker| {
            (
                worker.to_owned(),
                vec![(REPLACEMENT_HOST.to_owned(), fingerprint.to_owned())],
            )
        })
        .collect();
    let expected_gauge_after: BTreeMap<_, _> = worker_after
        .keys()
        .map(|worker| (worker.to_owned(), 2_000_000_000))
        .collect();
    let actual = (
        worker_after,
        wait_until(
            "a real TLS certificate after same-fingerprint replacement",
            Duration::from_secs(10),
            || tls_peer_fingerprint(listener_address, REPLACEMENT_HOST),
        ),
        worker_min_certificate_expirations(&mut instance),
    );
    let expected = (expected_worker_after, fingerprint, expected_gauge_after);
    assert_eq!(
        actual, expected,
        "same-fingerprint replacement must update worker names, real TLS and expiration"
    );
}

#[test]
#[ignore = "process-level: rejected certificate must preserve real TLS and every process state"]
fn rejected_add_certificate_with_invalid_key_preserves_tls_and_state() {
    assert_rejected_certificate_mutation_preserves_tls_and_state(
        "rejected-add-invalid-key",
        |listener, _certificate| {
            RequestType::AddCertificate(AddCertificate {
                address: listener,
                certificate: certificate_with_invalid_private_key(),
                expired_at: None,
            })
        },
    );
}

#[test]
#[ignore = "process-level: rejected certificate replacement must preserve real TLS and every process state"]
fn rejected_replace_certificate_with_invalid_key_preserves_tls_and_state() {
    assert_rejected_certificate_mutation_preserves_tls_and_state(
        "rejected-replace-invalid-key",
        |listener, certificate| {
            RequestType::ReplaceCertificate(ReplaceCertificate {
                address: listener,
                new_certificate: certificate_with_invalid_private_key(),
                old_fingerprint: certificate
                    .certificate
                    .fingerprint()
                    .expect("fingerprint original certificate")
                    .to_string(),
                new_expired_at: None,
            })
        },
    );
}

#[test]
#[ignore = "process-level: invalid SNI name must be refused before certificate state changes"]
fn rejected_add_certificate_with_invalid_name_preserves_tls_and_state() {
    assert_rejected_certificate_mutation_preserves_tls_and_state(
        "rejected-add-invalid-name",
        |listener, _certificate| {
            RequestType::AddCertificate(AddCertificate {
                address: listener,
                certificate: certificate_with_invalid_name(),
                expired_at: None,
            })
        },
    );
}

#[test]
#[ignore = "process-level: invalid SNI name must be refused before certificate replacement"]
fn rejected_replace_certificate_with_invalid_name_preserves_tls_and_state() {
    assert_rejected_certificate_mutation_preserves_tls_and_state(
        "rejected-replace-invalid-name",
        |listener, certificate| {
            RequestType::ReplaceCertificate(ReplaceCertificate {
                address: listener,
                new_certificate: certificate_with_invalid_name(),
                old_fingerprint: certificate
                    .certificate
                    .fingerprint()
                    .expect("fingerprint original certificate")
                    .to_string(),
                new_expired_at: None,
            })
        },
    );
}

#[test]
#[ignore = "process-level: certificate/private-key mismatch must not replace live TLS state"]
fn rejected_add_certificate_with_mismatched_key_preserves_tls_and_state() {
    assert_rejected_certificate_mutation_preserves_tls_and_state(
        "rejected-add-mismatched-key",
        |listener, _certificate| {
            RequestType::AddCertificate(AddCertificate {
                address: listener,
                certificate: certificate_with_mismatched_private_key(),
                expired_at: None,
            })
        },
    );
}

#[test]
#[ignore = "process-level: certificate/private-key mismatch must not replace live TLS state"]
fn rejected_replace_certificate_with_mismatched_key_preserves_tls_and_state() {
    assert_rejected_certificate_mutation_preserves_tls_and_state(
        "rejected-replace-mismatched-key",
        |listener, certificate| {
            RequestType::ReplaceCertificate(ReplaceCertificate {
                address: listener,
                new_certificate: certificate_with_mismatched_private_key(),
                old_fingerprint: certificate
                    .certificate
                    .fingerprint()
                    .expect("fingerprint original certificate")
                    .to_string(),
                new_expired_at: None,
            })
        },
    );
}

#[test]
#[ignore = "process-level: spawns a real main, worker, and TCP backend"]
fn tcp_listener_readdition_restores_frontend_and_routing() {
    let (backend_port, _backend) = spawn_tcp_backend();
    let listener_address = SocketAddr::from(([127, 0, 0, 1], free_tcp_port()));
    let listener: SocketAddress = listener_address.into();
    let cluster_id = "mutable-tcp-listener";
    let listener_config = ListenerBuilder::new_tcp(listener)
        .to_tcp(None)
        .expect("build TCP listener config");
    let frontend = RequestTcpFrontend {
        cluster_id: cluster_id.to_owned(),
        address: listener,
        tags: BTreeMap::from([("test".to_owned(), "mutable-runtime".to_owned())]),
        sni: None,
        alpn: Vec::new(),
    };
    let payload = b"tcp-listener-rehydrated";
    let mut instance = Instance::start(1);

    send_ok(
        &mut instance,
        RequestType::AddCluster(Cluster {
            cluster_id: cluster_id.to_owned(),
            ..Default::default()
        }),
    );
    send_ok(
        &mut instance,
        RequestType::AddTcpListener(listener_config.clone()),
    );
    send_ok(&mut instance, RequestType::AddTcpFrontend(frontend.clone()));
    send_ok(
        &mut instance,
        RequestType::AddBackend(AddBackend {
            cluster_id: cluster_id.to_owned(),
            backend_id: "tcp-listener".to_owned(),
            address: SocketAddr::from(([127, 0, 0, 1], backend_port)).into(),
            ..Default::default()
        }),
    );
    send_ok(
        &mut instance,
        RequestType::ActivateListener(ActivateListener {
            address: listener,
            proxy: ListenerType::Tcp as i32,
            from_scm: false,
            interface: None,
        }),
    );
    assert_roundtrip("initial TCP route", payload, || {
        tcp_roundtrip(listener_address, payload)
    });

    send_ok(
        &mut instance,
        RequestType::RemoveListener(RemoveListener {
            address: listener,
            proxy: ListenerType::Tcp as i32,
            interface: None,
        }),
    );
    assert!(saved_has_tcp_frontend(&mut instance, &frontend));
    for (process, cluster) in cluster_snapshots(&mut instance, cluster_id) {
        assert!(
            cluster.tcp_frontends.contains(&frontend),
            "{process} lost the retained TCP frontend"
        );
    }

    send_ok(&mut instance, RequestType::AddTcpListener(listener_config));
    send_ok(
        &mut instance,
        RequestType::ActivateListener(ActivateListener {
            address: listener,
            proxy: ListenerType::Tcp as i32,
            from_scm: false,
            interface: None,
        }),
    );
    assert_roundtrip("TCP route after listener re-addition", payload, || {
        tcp_roundtrip(listener_address, payload)
    });
}

/// Non-regression contract guard, not the RED oracle for the debug-only diff
/// postcondition: `LoadState` applies saved entries sequentially and production
/// never calls `ConfigState::diff`. It certifies that a real main and worker
/// keep saved frontend order, expose the same frontends, route traffic, and are
/// unchanged by a refused duplicate.
#[test]
#[ignore = "process-level: loads a saved state into a real main and worker and drives real TCP traffic through two listeners"]
fn tcp_load_state_preserves_frontend_order_routing_and_duplicate_refusal() {
    let (backend_port, _backend) = spawn_tcp_backend();
    // Hold both OS reservations at once so the kernel cannot hand out the same
    // ephemeral port twice; they are released just before `load_state`.
    let reservation_c = TcpListener::bind((HOST, 0)).expect("reserve TCP listener C");
    let reservation_a = TcpListener::bind((HOST, 0)).expect("reserve TCP listener A");
    let address_c: SocketAddr = reservation_c
        .local_addr()
        .expect("read reserved address for listener C");
    let address_a: SocketAddr = reservation_a
        .local_addr()
        .expect("read reserved address for listener A");
    assert_ne!(
        address_c, address_a,
        "the two listeners need distinct addresses"
    );
    let listener_c: SocketAddress = address_c.into();
    let listener_a: SocketAddress = address_a.into();
    let cluster_id = "mutable-tcp-load-order";
    let frontend = |listener: SocketAddress, route: &str| RequestTcpFrontend {
        cluster_id: cluster_id.to_owned(),
        address: listener,
        tags: BTreeMap::from([("route".to_owned(), route.to_owned())]),
        sni: None,
        alpn: Vec::new(),
    };
    let front_c = frontend(listener_c, "C");
    let front_a = frontend(listener_a, "A");
    let payload_c = b"load-order-c";
    let payload_a = b"load-order-a";

    let mut target = ConfigState::new();
    {
        let mut apply = |request_type: RequestType| {
            target
                .dispatch(&Request::from(request_type))
                .expect("the model admits the LoadState fixture");
        };
        apply(RequestType::AddCluster(Cluster {
            cluster_id: cluster_id.to_owned(),
            ..Default::default()
        }));
        apply(RequestType::AddBackend(AddBackend {
            cluster_id: cluster_id.to_owned(),
            backend_id: "tcp-load-order".to_owned(),
            address: SocketAddr::from(([127, 0, 0, 1], backend_port)).into(),
            ..Default::default()
        }));
        for listener in [listener_c, listener_a] {
            apply(RequestType::AddTcpListener(
                ListenerBuilder::new_tcp(listener)
                    .to_tcp(None)
                    .expect("build TCP listener config"),
            ));
            apply(RequestType::ActivateListener(ActivateListener {
                address: listener,
                proxy: ListenerType::Tcp as i32,
                from_scm: false,
                interface: None,
            }));
        }
        apply(RequestType::AddTcpFrontend(front_c.clone()));
        apply(RequestType::AddTcpFrontend(front_a.clone()));
    }
    let target_routes: Vec<String> = target.tcp_fronts[cluster_id]
        .iter()
        .map(|front| front.tags["route"].clone())
        .collect();
    assert_eq!(
        target_routes,
        ["C", "A"],
        "the fixture must store C before A"
    );

    let mut instance = Instance::start(1);
    // Release both reservations immediately before Sozu must bind them.
    drop(reservation_c);
    drop(reservation_a);
    instance.load_state(&target);

    let saved_frontends = |instance: &mut Instance| -> Vec<RequestTcpFrontend> {
        instance
            .state_requests()
            .into_iter()
            .filter_map(|request| match request {
                RequestType::AddTcpFrontend(frontend) => Some(frontend),
                _ => None,
            })
            .collect()
    };
    let assert_frontends =
        |instance: &mut Instance, expected: &[RequestTcpFrontend], stage: &str| {
            assert_eq!(
                saved_frontends(instance),
                expected,
                "{stage}: SaveState frontend order"
            );
            let snapshots = cluster_snapshots(instance, cluster_id);
            assert!(!snapshots.is_empty(), "{stage}: no per-process snapshots");
            for (process, cluster) in &snapshots {
                assert_eq!(
                    cluster.tcp_frontends, expected,
                    "{stage}: {process} TCP frontends"
                );
            }
        };
    let assert_route = |address: SocketAddr, payload: &'static [u8], description: &str| {
        assert_roundtrip(description, payload, || tcp_roundtrip(address, payload));
    };

    let loaded = [front_c.clone(), front_a.clone()];
    assert_frontends(&mut instance, &loaded, "after LoadState");
    assert_route(address_c, payload_c, "TCP route C after LoadState");
    assert_route(address_a, payload_a, "TCP route A after LoadState");

    // An exact duplicate is refused and changes neither state nor routing.
    let saved_before = saved_frontends(&mut instance);
    let snapshots_before = cluster_snapshots(&mut instance, cluster_id);
    let response = send_raw(
        &mut instance.control,
        RequestType::AddTcpFrontend(front_a.clone()),
        COMMAND_TIMEOUT,
    );
    assert_failure("duplicate AddTcpFrontend", &response);
    assert_eq!(saved_frontends(&mut instance), saved_before);
    assert_eq!(
        cluster_snapshots(&mut instance, cluster_id),
        snapshots_before,
        "a refused duplicate must leave every process snapshot unchanged"
    );
    assert_frontends(&mut instance, &loaded, "after refused duplicate");
    assert_route(address_c, payload_c, "TCP route C after refused duplicate");
    assert_route(address_a, payload_a, "TCP route A after refused duplicate");

    // Remove C: only A remains everywhere and A still routes.
    send_ok(
        &mut instance,
        RequestType::RemoveTcpFrontend(front_c.clone()),
    );
    assert_frontends(
        &mut instance,
        std::slice::from_ref(&front_a),
        "after removing C",
    );
    assert_route(address_a, payload_a, "TCP route A after removing C");

    // Re-add C: the accepted path appends it, giving the deterministic [A, C].
    send_ok(&mut instance, RequestType::AddTcpFrontend(front_c.clone()));
    let readded = [front_a.clone(), front_c.clone()];
    assert_frontends(&mut instance, &readded, "after re-adding C");
    assert_route(address_a, payload_a, "TCP route A after re-adding C");
    assert_route(address_c, payload_c, "TCP route C after re-adding C");
}

#[test]
#[ignore = "process-level: spawns a real main, worker, and UDP backend"]
fn udp_listener_readdition_restores_frontend_and_routing() {
    let (backend_port, _backend) = spawn_udp_backend();
    let listener_address = SocketAddr::from(([127, 0, 0, 1], free_udp_port()));
    let listener: SocketAddress = listener_address.into();
    let cluster_id = "mutable-udp-listener";
    let listener_config = ListenerBuilder::new_udp(listener)
        .to_udp(None)
        .expect("build UDP listener config");
    let frontend = RequestUdpFrontend {
        cluster_id: cluster_id.to_owned(),
        address: listener,
        tags: BTreeMap::from([("test".to_owned(), "mutable-runtime".to_owned())]),
    };
    let payload = b"udp-listener-rehydrated";
    let mut instance = Instance::start(1);

    send_ok(
        &mut instance,
        RequestType::AddCluster(Cluster {
            cluster_id: cluster_id.to_owned(),
            ..Default::default()
        }),
    );
    send_ok(
        &mut instance,
        RequestType::AddUdpListener(listener_config.clone()),
    );
    send_ok(&mut instance, RequestType::AddUdpFrontend(frontend.clone()));
    send_ok(
        &mut instance,
        RequestType::AddBackend(AddBackend {
            cluster_id: cluster_id.to_owned(),
            backend_id: "udp-listener".to_owned(),
            address: SocketAddr::from(([127, 0, 0, 1], backend_port)).into(),
            ..Default::default()
        }),
    );
    send_ok(
        &mut instance,
        RequestType::ActivateListener(ActivateListener {
            address: listener,
            proxy: ListenerType::Udp as i32,
            from_scm: false,
            interface: None,
        }),
    );
    assert_roundtrip("initial UDP route", payload, || {
        udp_roundtrip(listener_address, payload)
    });

    send_ok(
        &mut instance,
        RequestType::RemoveListener(RemoveListener {
            address: listener,
            proxy: ListenerType::Udp as i32,
            interface: None,
        }),
    );
    assert!(saved_has_udp_frontend(&mut instance, &frontend));
    for (process, cluster) in cluster_snapshots(&mut instance, cluster_id) {
        assert!(
            cluster.udp_frontends.contains(&frontend),
            "{process} lost the retained UDP frontend"
        );
    }

    send_ok(&mut instance, RequestType::AddUdpListener(listener_config));
    send_ok(
        &mut instance,
        RequestType::ActivateListener(ActivateListener {
            address: listener,
            proxy: ListenerType::Udp as i32,
            from_scm: false,
            interface: None,
        }),
    );
    assert_roundtrip("UDP route after listener re-addition", payload, || {
        udp_roundtrip(listener_address, payload)
    });
}
