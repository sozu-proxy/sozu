//! The public protected health check contract, against real masters and workers.
//!
//! The first test runs the published 2.3.1 main and worker. That main cannot
//! know `SetProtectedHealthCheck`, so it is never sent one: the candidate CLI
//! asks for the typed capability first, is refused, and must neither open the
//! header file nor leave any trace of it in the saved state, a listing, its own
//! output or the main's log. It needs the frozen release binary named by
//! `SOZU_CERTIFICATE_METADATA_LEGACY` and fails, never skips, without it.
//!
//! The second test specifies the public contract: through the real
//! `cluster health-check set --header-file`, a probe that was refused with 401
//! authenticates, the backend sees the exact `Authorization` bytes, a replaced
//! worker never probes without them, and a hot main upgrade keeps probes
//! authenticated and the header metadata, fingerprint included, byte for byte:
//! a worker started by the replacement main, before any new order, probes with
//! the transferred value, and submitting the same file again changes nothing.
//! The value appears in no output, log, listing or saved state.
//!
//! The third test boots from a TOML `value_file`, saves the state, restarts
//! over that state and checks the saved, foreign fingerprint stays the
//! authority: no probe is sent until the same file is submitted again.
//!
//! Process-level, so `#[ignore]`d like the other master/worker suites; CI runs
//! it explicitly:
//!
//! ```bash
//! SOZU_CERTIFICATE_METADATA_LEGACY=/path/to/sozu-2.3.1 \
//!     cargo test -p sozu --test protected_health_check_refusal_e2e -- --ignored
//! ```

use std::{
    collections::BTreeMap,
    fs::File,
    io::{ErrorKind, Read, Write},
    net::{SocketAddr, TcpListener},
    os::unix::{fs::PermissionsExt, process::CommandExt},
    path::{Path, PathBuf},
    process::{Child, Command, Output, Stdio},
    sync::{
        Arc, Mutex,
        atomic::{AtomicBool, Ordering},
    },
    thread::{self, JoinHandle},
    time::{Duration, Instant},
};

use nix::{
    sys::signal::{Signal, kill},
    unistd::Pid,
};
use sha2::{Digest, Sha256};
use sozu_command_lib::{
    channel::Channel,
    proto::command::{
        AddBackend, Cluster, HealthCheckHeader, ListWorkers, QueryHealthChecks,
        QueryMetricsOptions, Request, Response, ResponseStatus, RunState, UpgradeMain,
        filtered_metrics::Inner, request::RequestType, response_content::ContentType,
    },
};
use sozu_lib::metrics::names::health_check::{DOWN, FAILURE, MISSING_SECRET, SUCCESS, UP};

/// The published 2.3.1 release the first test launches as the main process.
const LEGACY_SHA256: &str = "5ea1452b3a2053cdc9936dc50ce25d50e3ad30122c7f49fcc606863cd93b2281";
const LEGACY_VERSION: &str = "sozu 2.3.1 (1cc9bf2";
/// Unique to the first test: the header file path it points the CLI at.
const ABSENT_MARKER: &str = "SYNTHETIC-HC-ABSENT-VALUE-PATH-7b3f";
const OPEN_FAILURE: &str = "could not open the health check header file";
const TOKEN: &str = "SYNTHETIC-HC-E2E-PROBE-TOKEN-4d8a";
const BUFFER: u64 = 4096;
const MAX_BUFFER: u64 = 163_840;
const VERDICT_BUDGET: Duration = Duration::from_secs(20);

/// The spawned master, in its own process group: every exit from a test
/// terminates the whole group (master and workers) and reaps the master.
struct MasterProcess {
    child: Child,
    process_group: i32,
}

impl MasterProcess {
    fn spawn(command: &mut Command) -> Self {
        let child = command.process_group(0).spawn().expect("spawn sozu start");
        let process_group = child.id() as i32;
        eprintln!("owned main process group {process_group}");
        Self {
            child,
            process_group,
        }
    }
}

impl Drop for MasterProcess {
    fn drop(&mut self) {
        let group = Pid::from_raw(-self.process_group);
        let _ = kill(group, Signal::SIGTERM);
        let deadline = Instant::now() + Duration::from_secs(3);
        while Instant::now() < deadline {
            let _ = self.child.try_wait();
            if kill(group, None).is_err() {
                return;
            }
            thread::sleep(Duration::from_millis(25));
        }
        let _ = kill(group, Signal::SIGKILL);
        let _ = self.child.wait();
        while kill(group, None).is_ok() && Instant::now() < deadline + Duration::from_secs(3) {
            thread::sleep(Duration::from_millis(25));
        }
    }
}

struct Instance {
    _master: MasterProcess,
    temp: Arc<tempfile::TempDir>,
    config_arg: String,
    log_path: PathBuf,
    pid_path: PathBuf,
}

impl Instance {
    /// A candidate main process.
    fn start() -> Self {
        Self::start_with_binary(Path::new(env!("CARGO_BIN_EXE_sozu")))
    }

    /// A main process launched from `binary`; the CLI that drives it is always
    /// the candidate's.
    fn start_with_binary(binary: &Path) -> Self {
        let temp = Arc::new(tempfile::tempdir().expect("tempdir"));
        Self::launch(binary, temp, "master.log", "")
    }

    /// A candidate main process in `temp`, which an earlier main process, long
    /// since dropped and reaped, may have shared: it reads the same
    /// configuration directory and inputs and logs to `log_name`. `extra` is
    /// appended to the configuration, top-level keys before tables.
    fn launch(binary: &Path, temp: Arc<tempfile::TempDir>, log_name: &str, extra: &str) -> Self {
        let socket_path = temp.path().join("sozu.sock");
        let config_path = temp.path().join("config.toml");
        let log_path = temp.path().join(log_name);
        let pid_path = temp.path().join("sozu.pid");
        // What a reaped predecessor left behind is not this main's.
        let _ = std::fs::remove_file(&socket_path);
        let _ = std::fs::remove_file(&pid_path);
        std::fs::write(
            &config_path,
            format!(
                "command_socket = \"{}\"\ncommand_buffer_size = {BUFFER}\n\
                 max_command_buffer_size = {MAX_BUFFER}\nworker_count = 1\n\
                 worker_automatic_restart = false\nworker_timeout = 5\n\
                 ctl_command_timeout = 30000\nlog_level = \"debug\"\nlog_target = \"stdout\"\n\
                 max_connections = 100\nbuffer_size = 16393\npid_file_path = \"{}\"\n{extra}",
                socket_path.display(),
                pid_path.display()
            ),
        )
        .expect("write config");
        let log = File::create(&log_path).expect("create master log");
        let log_err = log.try_clone().expect("clone master log handle");
        let master = MasterProcess::spawn(
            Command::new(binary)
                .args(["start", "-c", config_path.to_str().expect("utf-8 path")])
                .stdout(Stdio::from(log))
                .stderr(Stdio::from(log_err)),
        );
        let instance = Self {
            _master: master,
            temp,
            config_arg: config_path.to_str().expect("utf-8 path").to_owned(),
            log_path,
            pid_path,
        };
        // A worker must answer `status`: an order refused with no worker alive
        // would prove nothing about the worker side.
        let deadline = Instant::now() + Duration::from_secs(10);
        while !instance.run(&["status"]).status.success() {
            assert!(
                Instant::now() < deadline,
                "no worker answered `status` in time:\n{}",
                instance.master_log()
            );
            std::thread::sleep(Duration::from_millis(100));
        }
        instance
    }

    fn master_log(&self) -> String {
        String::from_utf8_lossy(&std::fs::read(&self.log_path).unwrap_or_default()).into_owned()
    }

    /// The candidate CLI, pointed at this instance's command socket.
    fn cli(&self, args: &[&str]) -> Command {
        let mut command = Command::new(env!("CARGO_BIN_EXE_sozu"));
        command.args(["-c", &self.config_arg]).args(args);
        command
    }

    fn run(&self, args: &[&str]) -> Output {
        self.cli(args).output().expect("run CLI")
    }

    /// Send one raw request and return its final (non-`Processing`) answer.
    fn send(&self, request: Request) -> Response {
        let mut channel: Channel<Request, Response> = Channel::from_path(
            self.temp
                .path()
                .join("sozu.sock")
                .to_str()
                .expect("utf-8 path"),
            BUFFER,
            MAX_BUFFER,
        )
        .expect("connect to the command socket");
        channel.blocking().expect("blocking channel");
        channel.write_message(&request).expect("send the request");
        loop {
            let response = channel
                .read_message_blocking_timeout(Some(Duration::from_secs(20)))
                .expect("the master answers");
            if response.status != ResponseStatus::Processing as i32 {
                return response;
            }
        }
    }

    fn main_pid(&self) -> u32 {
        std::fs::read_to_string(&self.pid_path)
            .expect("read the main pid file")
            .trim()
            .parse()
            .expect("parse the main pid")
    }

    /// Replace the main process in place and return the new one's pid. The
    /// workers stay; the replacement must answer on the same command socket
    /// and sit in the process group the test owns, so its cleanup reaps both
    /// generations.
    fn upgrade_main(&self) -> u32 {
        let old_main = self.main_pid();
        let upgraded = self.send(RequestType::UpgradeMain(UpgradeMain {}).into());
        assert_eq!(
            upgraded.status,
            ResponseStatus::Ok as i32,
            "{upgraded:?}\n{}",
            self.master_log()
        );
        let deadline = Instant::now() + Duration::from_secs(10);
        let new_main = loop {
            let pid = self.main_pid();
            if pid != old_main && Path::new(&format!("/proc/{pid}")).exists() {
                break pid;
            }
            assert!(
                Instant::now() < deadline,
                "the main pid file still names {pid}:\n{}",
                self.master_log()
            );
            thread::sleep(Duration::from_millis(50));
        };
        assert_eq!(
            process_group_of(new_main),
            self._master.process_group,
            "the replacement main {new_main} left the owned process group"
        );
        while !self.run(&["status"]).status.success() {
            assert!(
                Instant::now() < deadline,
                "the replacement main never answered `status`:\n{}",
                self.master_log()
            );
            thread::sleep(Duration::from_millis(100));
        }
        new_main
    }
}

/// The process group of `pid`, from the `pgrp` field of `/proc/<pid>/stat`
/// (the one after the parenthesised command name, which may hold spaces).
fn process_group_of(pid: u32) -> i32 {
    let stat = std::fs::read_to_string(format!("/proc/{pid}/stat")).expect("read process stat");
    let (_, after_name) = stat
        .rsplit_once(") ")
        .expect("process stat has a command name");
    after_name
        .split_whitespace()
        .nth(2)
        .and_then(|group| group.parse().ok())
        .expect("process stat has a process group")
}

fn output_text(output: &Output) -> String {
    format!(
        "{}{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    )
}

/// The published 2.3.1 binary named by `SOZU_CERTIFICATE_METADATA_LEGACY`,
/// checked against the release it must be: a missing, mismatched or
/// unlaunchable fixture fails the test, it never skips it.
fn legacy_binary() -> PathBuf {
    let path = PathBuf::from(
        std::env::var_os("SOZU_CERTIFICATE_METADATA_LEGACY")
            .filter(|path| !path.is_empty())
            .expect("set SOZU_CERTIFICATE_METADATA_LEGACY to the frozen 2.3.1 binary"),
    );
    let bytes = std::fs::read(&path).expect("read the legacy 2.3.1 binary");
    let digest: String = Sha256::digest(&bytes)
        .iter()
        .map(|byte| format!("{byte:02x}"))
        .collect();
    assert_eq!(
        digest,
        LEGACY_SHA256,
        "{} is not the 2.3.1 release",
        path.display()
    );
    let version = Command::new(&path)
        .arg("--version")
        .output()
        .expect("launch the legacy 2.3.1 binary");
    assert!(
        String::from_utf8_lossy(&version.stdout).contains(LEGACY_VERSION),
        "{version:?}"
    );
    path
}

/// A published 2.3.1 main process cannot know `SetProtectedHealthCheck`, so
/// the candidate CLI must ask for the typed capability first, be refused, and
/// stop: it never opens the header file (here a path that does not exist, so
/// opening it would be an error) and nothing about the path reaches the main,
/// its saved state, a listing or any output.
#[test]
#[ignore = "process-level: spawns a real published 2.3.1 master and worker; run from the dedicated CI step or with --ignored (see module docs)"]
fn published_2_3_1_main_refuses_header_file_set_before_any_file_io_and_keeps_no_trace() {
    let instance = Instance::start_with_binary(&legacy_binary());
    let added = instance.send(
        RequestType::AddCluster(Cluster {
            cluster_id: "app".to_owned(),
            ..Default::default()
        })
        .into(),
    );
    assert_eq!(added.status, ResponseStatus::Ok as i32, "{added:?}");

    let absent = instance.temp.path().join(ABSENT_MARKER);
    let spec = format!("Authorization={}", absent.display());
    // At the default `info` level: the config's `debug` would have the CLI
    // echo the parsed command, argument path included, which says nothing of
    // what the capability query decided.
    let set = instance
        .cli(&[
            "cluster",
            "health-check",
            "set",
            "--id",
            "app",
            "--header-file",
            spec.as_str(),
        ])
        .env("RUST_LOG", "info")
        .output()
        .expect("run CLI");
    let set_output = output_text(&set);
    assert!(!set.status.success(), "the set was accepted:\n{set_output}");
    assert!(
        !set_output.trim().is_empty(),
        "a refusal must say something: {set:?}"
    );
    assert!(
        set_output.contains(
            "the main process did not confirm support for protected health check headers: nothing was read or sent"
        ) && set_output.contains("ProtectedHealthCheckUnsupported"),
        "the failure is not the typed capability refusal:\n{set_output}"
    );
    assert!(
        !set_output.contains(OPEN_FAILURE)
            && !set_output.to_lowercase().contains("no such file")
            && !set_output.contains("os error"),
        "the header file was opened before the capability was confirmed:\n{set_output}"
    );
    assert!(!absent.exists(), "the absent path was created");

    // Nothing was applied, kept or persisted. The cluster itself is saved.
    let saved_path = instance.temp.path().join("out.state");
    let save = instance.run(&[
        "state",
        "save",
        "-f",
        saved_path.to_str().expect("utf-8 path"),
    ]);
    assert!(save.status.success(), "{}", output_text(&save));
    let saved =
        String::from_utf8_lossy(&std::fs::read(&saved_path).expect("saved state")).into_owned();
    assert!(saved.contains("\"app\""), "the cluster is saved: {saved}");
    let listing = output_text(&instance.run(&["cluster", "health-check", "list"]));
    let json_listing = output_text(&instance.run(&["--json", "cluster", "health-check", "list"]));
    for (surface, text) in [
        ("the CLI output", set_output),
        ("the saved state", saved),
        ("the human listing", listing),
        ("the JSON listing", json_listing),
        ("the main log", instance.master_log()),
    ] {
        for needle in [ABSENT_MARKER, "Authorization", "SetProtectedHealthCheck"] {
            assert!(!text.contains(needle), "{surface} holds {needle}: {text}");
        }
    }
}

/// A probe backend answering 200 to the exact `expected` `Authorization` and
/// 401 to any other request, recording the `Authorization` of each request it
/// reads (`None` for a request without one). Stopped and joined on drop.
struct AuthorizationBackend {
    address: SocketAddr,
    seen: Arc<Mutex<Vec<Option<Vec<u8>>>>>,
    stop: Arc<AtomicBool>,
    handle: Option<JoinHandle<()>>,
}

impl AuthorizationBackend {
    fn start(expected: &str) -> Self {
        let listener = TcpListener::bind("127.0.0.1:0").expect("bind probe backend");
        listener.set_nonblocking(true).expect("nonblocking backend");
        let address = listener.local_addr().expect("probe backend address");
        let seen = Arc::new(Mutex::new(Vec::new()));
        let stop = Arc::new(AtomicBool::new(false));
        let (recorded, stopped, expected) = (
            Arc::clone(&seen),
            Arc::clone(&stop),
            expected.as_bytes().to_vec(),
        );
        let handle = thread::spawn(move || {
            while !stopped.load(Ordering::Acquire) {
                let mut stream = match listener.accept() {
                    Ok((stream, _)) => stream,
                    Err(error) if error.kind() == ErrorKind::WouldBlock => {
                        thread::sleep(Duration::from_millis(10));
                        continue;
                    }
                    Err(_) => return,
                };
                let _ = stream.set_nonblocking(false);
                let _ = stream.set_read_timeout(Some(Duration::from_secs(2)));
                let mut request = Vec::new();
                let mut chunk = [0u8; 1024];
                while !request.windows(4).any(|window| window == b"\r\n\r\n") {
                    match stream.read(&mut chunk) {
                        Ok(0) | Err(_) => break,
                        Ok(read) => request.extend_from_slice(&chunk[..read]),
                    }
                }
                if request.is_empty() {
                    continue;
                }
                let authorization = request
                    .split(|byte| *byte == b'\n')
                    .find_map(|line| line.strip_prefix(b"Authorization: "))
                    .map(|value| value.strip_suffix(b"\r").unwrap_or(value).to_vec());
                let status = if authorization.as_deref() == Some(expected.as_slice()) {
                    "200 OK"
                } else {
                    "401 Unauthorized"
                };
                recorded
                    .lock()
                    .expect("log not poisoned")
                    .push(authorization);
                let _ = stream.write_all(
                    format!("HTTP/1.1 {status}\r\nContent-Length: 0\r\nConnection: close\r\n\r\n")
                        .as_bytes(),
                );
            }
        });
        Self {
            address,
            seen,
            stop,
            handle: Some(handle),
        }
    }

    fn seen(&self) -> Vec<Option<Vec<u8>>> {
        self.seen.lock().expect("log not poisoned").clone()
    }
}

impl Drop for AuthorizationBackend {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::Release);
        if let Some(handle) = self.handle.take() {
            let _ = handle.join();
        }
    }
}

/// The named counters of each worker by worker id, read through the main
/// process; an absent counter reads as 0.
fn worker_counts(instance: &Instance, names: &[&str]) -> BTreeMap<String, Vec<i64>> {
    let response = instance.send(
        RequestType::QueryMetrics(QueryMetricsOptions {
            list: false,
            cluster_ids: vec![],
            backend_ids: vec![],
            metric_names: names.iter().map(|name| (*name).to_owned()).collect(),
            no_clusters: true,
            workers: true,
        })
        .into(),
    );
    assert_eq!(response.status, ResponseStatus::Ok as i32, "{response:?}");
    let Some(ContentType::Metrics(metrics)) = response
        .content
        .as_ref()
        .and_then(|content| content.content_type.as_ref())
    else {
        panic!("QueryMetrics must answer with aggregated metrics: {response:?}");
    };
    metrics
        .workers
        .iter()
        .map(|(worker, metrics)| {
            let count = |name: &str| match metrics.proxy.get(name).and_then(|m| m.inner.as_ref()) {
                Some(Inner::Count(value)) => *value,
                _ => 0,
            };
            (
                worker.to_owned(),
                names.iter().map(|name| count(name)).collect(),
            )
        })
        .collect()
}

/// Each worker's `[success, failure, down, up]` health counters by worker id.
fn health_counters(instance: &Instance) -> BTreeMap<String, [i64; 4]> {
    worker_counts(instance, &[SUCCESS, FAILURE, DOWN, UP])
        .into_iter()
        .map(|(worker, counts)| (worker, [counts[0], counts[1], counts[2], counts[3]]))
        .collect()
}

/// Poll the counters until `done` holds, within the verdict budget.
fn wait_for_counters(
    instance: &Instance,
    what: &str,
    done: impl Fn(&BTreeMap<String, [i64; 4]>) -> bool,
) -> BTreeMap<String, [i64; 4]> {
    let deadline = Instant::now() + VERDICT_BUDGET;
    loop {
        let sample = health_counters(instance);
        if done(&sample) {
            return sample;
        }
        assert!(
            Instant::now() < deadline,
            "timed out waiting for {what}: {sample:?}"
        );
        thread::sleep(Duration::from_millis(100));
    }
}

/// The counters summed over every worker.
fn total(sample: &BTreeMap<String, [i64; 4]>) -> [i64; 4] {
    sample.values().fold([0; 4], |sum, counters| {
        std::array::from_fn(|index| sum[index] + counters[index])
    })
}

/// The `(id, pid)` of every running worker.
fn running_workers(instance: &Instance) -> Vec<(u32, u32)> {
    let response = instance.send(RequestType::ListWorkers(ListWorkers {}).into());
    let Some(ContentType::Workers(workers)) = response
        .content
        .as_ref()
        .and_then(|content| content.content_type.as_ref())
    else {
        panic!("ListWorkers must answer with workers: {response:?}");
    };
    workers
        .vec
        .iter()
        .filter(|worker| worker.run_state == RunState::Running as i32 && worker.pid > 0)
        .map(|worker| (worker.id, worker.pid as u32))
        .collect()
}

/// The typed header metadata of cluster `app`, in the order it was declared:
/// names and complete fingerprints, never a value.
fn health_check_headers(instance: &Instance) -> Vec<HealthCheckHeader> {
    let listed = instance.send(
        RequestType::QueryHealthChecks(QueryHealthChecks {
            cluster_id: Some("app".to_owned()),
        })
        .into(),
    );
    let Some(ContentType::HealthChecksList(list)) = listed
        .content
        .as_ref()
        .and_then(|content| content.content_type.as_ref())
    else {
        panic!("QueryHealthChecks must answer with a listing: {listed:?}");
    };
    list.map["app"].headers.clone()
}

/// The value is on no surface an operator can read: the output of the `set`
/// that carried it, a listing, the saved state or the master and worker log.
/// The header name and its fingerprint are metadata and are shown.
fn assert_value_absent_everywhere(instance: &Instance, set_output: &str) {
    let saved_path = instance.temp.path().join("out.state");
    let save = instance.run(&["state", "save", "-f", saved_path.to_str().expect("utf-8")]);
    assert!(save.status.success(), "{}", output_text(&save));
    let saved =
        String::from_utf8_lossy(&std::fs::read(&saved_path).expect("saved state")).into_owned();
    let human = output_text(&instance.run(&["cluster", "health-check", "list"]));
    let json = output_text(&instance.run(&["--json", "cluster", "health-check", "list"]));
    let listed = instance.send(
        RequestType::QueryHealthChecks(QueryHealthChecks {
            cluster_id: Some("app".to_owned()),
        })
        .into(),
    );
    let Some(ContentType::HealthChecksList(list)) = listed
        .content
        .as_ref()
        .and_then(|content| content.content_type.as_ref())
    else {
        panic!("QueryHealthChecks must answer with a listing: {listed:?}");
    };
    let headers = &list.map["app"].headers;
    assert_eq!(headers.len(), 1, "{headers:?}");
    assert_eq!(headers[0].name, "Authorization");
    let fingerprint = headers[0].fingerprint.as_str();
    assert!(
        fingerprint.starts_with("sozu-hc-hmac-sha256:v1:"),
        "{fingerprint}"
    );
    for (surface, text) in [
        ("the set output", set_output.to_owned()),
        ("the human listing", human),
        ("the JSON listing", json.to_owned()),
        ("the typed listing", format!("{listed:?}")),
        ("the saved state", saved.to_owned()),
        ("the master log", instance.master_log()),
    ] {
        assert!(!text.contains(TOKEN), "{surface} shows the value: {text}");
    }
    for (surface, text) in [("the JSON listing", &json), ("the saved state", &saved)] {
        assert!(
            text.contains("Authorization") && text.contains(fingerprint),
            "{surface} lacks the header metadata: {text}"
        );
    }
}

/// Through the public CLI a cluster whose probe port demands an
/// `Authorization` goes from 401/DOWN to 200/UP, the backend reads the exact
/// bytes, and the value stays out of every surface. A replaced worker's probes
/// all authenticate: none fails, none reaches the backend without the value.
#[test]
#[ignore = "process-level: spawns a real master and worker; run from the dedicated CI step or with --ignored (see module docs)"]
fn public_protected_health_check_authenticates_probes_and_leaks_nothing() {
    let instance = Instance::start();
    let authorization = format!("Bearer {TOKEN}");
    let backend = AuthorizationBackend::start(&authorization);
    // The data port is closed: only the dedicated probe port serves.
    let data_address = TcpListener::bind("127.0.0.1:0")
        .and_then(|listener| listener.local_addr())
        .expect("reserve a closed data port");
    for request in [
        RequestType::AddCluster(Cluster {
            cluster_id: "app".to_owned(),
            ..Default::default()
        }),
        RequestType::AddBackend(AddBackend {
            cluster_id: "app".to_owned(),
            backend_id: "app-0".to_owned(),
            address: data_address.into(),
            ..Default::default()
        }),
    ] {
        let response = instance.send(request.into());
        assert_eq!(response.status, ResponseStatus::Ok as i32, "{response:?}");
    }
    let port = backend.address.port().to_string();
    let set = |extra: &[&str]| {
        let mut args = vec![
            "cluster",
            "health-check",
            "set",
            "--id",
            "app",
            "--port",
            port.as_str(),
            "--interval",
            "1",
            "--probe-timeout",
            "1",
            "--healthy-threshold",
            "1",
            "--unhealthy-threshold",
            "1",
        ];
        args.extend_from_slice(extra);
        instance.run(&args)
    };

    // Without the credential the probe is a 401: failure and DOWN, never UP.
    let headerless = set(&[]);
    assert!(headerless.status.success(), "{}", output_text(&headerless));
    let sample = wait_for_counters(&instance, "a 401 probe and the DOWN verdict", |sample| {
        let [_, failure, down, _] = total(sample);
        failure >= 1 && down >= 1
    });
    let [success, _, down, up] = total(&sample);
    assert_eq!((success, down, up), (0, 1, 0), "{sample:?}");
    let headerless_probes = backend.seen();
    assert!(
        !headerless_probes.is_empty() && headerless_probes.iter().all(Option::is_none),
        "{headerless_probes:?}"
    );

    // The public order, from a file: 200, success and UP, with the exact bytes.
    let value_file = instance.temp.path().join("authorization.value");
    std::fs::write(&value_file, format!("{authorization}\n")).expect("write the value file");
    std::fs::set_permissions(&value_file, std::fs::Permissions::from_mode(0o600))
        .expect("restrict the value file");
    let spec = format!("Authorization={}", value_file.display());
    let protected = set(&["--header-file", spec.as_str()]);
    let set_output = output_text(&protected);
    assert!(
        protected.status.success(),
        "the public protected set failed:\n{set_output}"
    );
    let sample = wait_for_counters(&instance, "a 200 probe and the UP verdict", |sample| {
        let [success, _, _, up] = total(sample);
        success >= 1 && up >= 1
    });
    let [_, _, down, up] = total(&sample);
    assert_eq!((down, up), (1, 1), "{sample:?}");
    assert!(
        backend
            .seen()
            .contains(&Some(authorization.as_bytes().to_vec())),
        "the backend never read the exact value: {:?}",
        backend.seen()
    );
    assert_value_absent_everywhere(&instance, &set_output);

    // A replacement worker is not ready before its probes authenticate: from
    // its own counters, none failed and none went DOWN, and the backend saw no
    // request without the value since the replacement began.
    let [(worker_id, old_pid)] = running_workers(&instance)[..] else {
        panic!("the test needs exactly one running worker");
    };
    eprintln!("original worker {worker_id} pid {old_pid}");
    let mark = backend.seen().len();
    let upgraded = instance.send(RequestType::UpgradeWorker(worker_id).into());
    assert_eq!(upgraded.status, ResponseStatus::Ok as i32, "{upgraded:?}");
    let [(new_id, new_pid)] = running_workers(&instance)[..] else {
        panic!("the upgrade must leave exactly one running worker");
    };
    eprintln!("replacement worker {new_id} pid {new_pid}");
    assert_ne!(new_pid, old_pid, "the worker was not replaced");
    let key = new_id.to_string();
    let sample = wait_for_counters(&instance, "two probes of the replacement", |sample| {
        sample.get(&key).is_some_and(|counters| counters[0] >= 2)
    });
    let [_, failure, down, _] = sample[&key];
    assert_eq!((failure, down), (0, 0), "{sample:?}");
    let replacement_probes = backend.seen().split_off(mark);
    assert!(
        !replacement_probes.is_empty()
            && replacement_probes
                .iter()
                .all(|seen| seen.as_deref() == Some(authorization.as_bytes())),
        "a probe since the replacement lacked the value: {replacement_probes:?}"
    );
    assert_value_absent_everywhere(&instance, &set_output);

    // A hot main upgrade keeps the workers and the secret with them: the
    // header metadata, fingerprint included, is exactly what it was, and every
    // probe from the mark on carries the exact bytes while success keeps
    // counting and nothing fails or goes DOWN.
    let workers = running_workers(&instance);
    let metadata = health_check_headers(&instance);
    let before = health_counters(&instance)
        .get(&key)
        .copied()
        .expect("the worker reports health counters");
    let mark = backend.seen().len();
    let new_main = instance.upgrade_main();
    eprintln!("replacement main pid {new_main}");
    assert_eq!(
        running_workers(&instance),
        workers,
        "a main upgrade must keep the workers"
    );
    assert_eq!(
        health_check_headers(&instance),
        metadata,
        "the header metadata changed across the main upgrade"
    );
    let sample = wait_for_counters(&instance, "probes after the main upgrade", |sample| {
        sample
            .get(&key)
            .is_some_and(|counters| counters[0] >= before[0] + 2)
    });
    let [success, failure, down, up] = sample[&key];
    assert!(success >= before[0] + 2, "{sample:?}");
    assert_eq!(
        (failure, down, up),
        (before[1], before[2], before[3]),
        "a probe failed or flapped across the main upgrade: {sample:?}"
    );
    let upgraded_probes = backend.seen().split_off(mark);
    assert!(
        !upgraded_probes.is_empty()
            && upgraded_probes
                .iter()
                .all(|seen| seen.as_deref() == Some(authorization.as_bytes())),
        "a probe since the main upgrade lacked the value: {upgraded_probes:?}"
    );
    assert_eq!(
        health_check_headers(&instance),
        metadata,
        "the header metadata changed once probes resumed"
    );
    assert_value_absent_everywhere(&instance, &set_output);

    // The replacement main owns the transferred confirmed values: a worker it
    // starts, before any new order, probes with the exact bytes and never
    // without them.
    let [(hot_id, hot_pid)] = running_workers(&instance)[..] else {
        panic!("the test needs exactly one running worker");
    };
    let mark = backend.seen().len();
    let upgraded = instance.send(RequestType::UpgradeWorker(hot_id).into());
    assert_eq!(
        upgraded.status,
        ResponseStatus::Ok as i32,
        "{upgraded:?}\n{}",
        instance.master_log()
    );
    let [(post_id, post_pid)] = running_workers(&instance)[..] else {
        panic!("the upgrade must leave exactly one running worker");
    };
    eprintln!("post-hot replacement worker {post_id} pid {post_pid}");
    assert!(
        post_pid > 0 && post_pid != hot_pid,
        "{hot_pid} -> {post_pid}"
    );
    assert!(post_id > 0 && post_id != hot_id, "{hot_id} -> {post_id}");
    let post_key = post_id.to_string();
    let sample = wait_for_counters(
        &instance,
        "two probes of the post-hot replacement",
        |sample| {
            sample
                .get(&post_key)
                .is_some_and(|counters| counters[0] >= 2)
        },
    );
    let [_, failure, down, _] = sample[&post_key];
    assert_eq!((failure, down), (0, 0), "{sample:?}");
    let post_hot_probes = backend.seen().split_off(mark);
    assert!(
        !post_hot_probes.is_empty()
            && post_hot_probes
                .iter()
                .all(|seen| seen.as_deref() == Some(authorization.as_bytes())),
        "a probe since the post-hot replacement lacked the value: {post_hot_probes:?}"
    );

    // The same header file submitted again yields the same value under the
    // same key: the complete ordered metadata, fingerprint included, is
    // byte for byte what it was, and probes keep authenticating.
    let resubmitted = set(&["--header-file", spec.as_str()]);
    let resubmitted_output = output_text(&resubmitted);
    assert!(
        resubmitted.status.success(),
        "the repeated protected set failed:\n{resubmitted_output}"
    );
    assert_eq!(
        health_check_headers(&instance),
        metadata,
        "the repeated header file changed the metadata"
    );
    let before = health_counters(&instance)
        .get(&post_key)
        .copied()
        .expect("the post-hot worker reports health counters");
    let mark = backend.seen().len();
    let sample = wait_for_counters(&instance, "probes after the repeated set", |sample| {
        sample
            .get(&post_key)
            .is_some_and(|counters| counters[0] >= before[0] + 2)
    });
    let [_, failure, down, _] = sample[&post_key];
    assert_eq!((failure, down), (0, 0), "{sample:?}");
    let repeated_probes = backend.seen().split_off(mark);
    assert!(
        !repeated_probes.is_empty()
            && repeated_probes
                .iter()
                .all(|seen| seen.as_deref() == Some(authorization.as_bytes())),
        "a probe since the repeated set lacked the value: {repeated_probes:?}"
    );
    assert_eq!(
        health_check_headers(&instance),
        metadata,
        "the header metadata changed once probes resumed after the repeated set"
    );
    assert_value_absent_everywhere(&instance, &set_output);
    assert_value_absent_everywhere(&instance, &resubmitted_output);
}

/// Every regular file under `dir` that holds `needle`, as a path relative to it.
fn files_holding(dir: &Path, needle: &[u8]) -> Vec<PathBuf> {
    fn walk(root: &Path, dir: &Path, needle: &[u8], found: &mut Vec<PathBuf>) {
        for entry in std::fs::read_dir(dir)
            .expect("read the directory")
            .flatten()
        {
            let path = entry.path();
            match entry.file_type().expect("file type") {
                kind if kind.is_dir() => walk(root, &path, needle, found),
                kind if kind.is_file() => {
                    let bytes = std::fs::read(&path).expect("read a file");
                    if bytes.windows(needle.len()).any(|window| window == needle) {
                        found.push(path.strip_prefix(root).expect("under root").to_owned());
                    }
                }
                _ => {}
            }
        }
    }
    let mut found = Vec::new();
    walk(dir, dir, needle, &mut found);
    found
}

/// Cold TOML, a real `state save` and a restart. Boot one applies the
/// configuration's `value_file` and authenticates. Boot two loads the same
/// configuration over the state saved by boot one: that state is the
/// authority of the cluster, its fingerprint is foreign to the new main, and
/// the configuration's value is not applied under it. The workers hold no
/// value, so no probe is sent until the same file is submitted through the
/// public CLI, which yields a new local fingerprint and authenticated probes.
#[test]
#[ignore = "process-level: spawns real masters and workers; run from the dedicated CI step or with --ignored (see module docs)"]
fn cold_toml_header_file_does_not_override_saved_foreign_metadata_after_restart() {
    let temp = Arc::new(tempfile::tempdir().expect("tempdir"));
    let authorization = format!("Bearer {TOKEN}");
    let backend = AuthorizationBackend::start(&authorization);
    let data_address = TcpListener::bind("127.0.0.1:0")
        .and_then(|listener| listener.local_addr())
        .expect("reserve a closed data port");
    let value_file = temp.path().join("authorization.value");
    std::fs::write(&value_file, format!("{authorization}\n")).expect("write the value file");
    std::fs::set_permissions(&value_file, std::fs::Permissions::from_mode(0o600))
        .expect("restrict the value file");
    let value_path = value_file.to_str().expect("utf-8 path").to_owned();
    let port = backend.address.port().to_string();
    let saved_path = temp.path().join("cold.state");
    let cluster = format!(
        "[clusters.app]\nprotocol = \"http\"\nfrontends = []\n\
         backends = [{{ address = \"{data_address}\", backend_id = \"app-0\" }}]\n\n\
         [clusters.app.health_check]\nuri = \"/\"\ninterval = 1\ntimeout = 1\n\
         healthy_threshold = 1\nunhealthy_threshold = 1\nport = {port}\n\n\
         [[clusters.app.health_check.headers]]\nname = \"Authorization\"\n\
         value_file = \"{value_path}\"\n"
    );
    let binary = Path::new(env!("CARGO_BIN_EXE_sozu"));
    let mut outputs = Vec::new();

    // Boot one: the configuration alone authenticates the probe.
    let first = Instance::launch(binary, Arc::clone(&temp), "master-1.log", &cluster);
    let sample = wait_for_counters(&first, "a 200 probe from the configuration", |sample| {
        total(sample)[0] >= 1
    });
    let [success, failure, down, _] = total(&sample);
    assert!(success >= 1 && failure == 0 && down == 0, "{sample:?}");
    let seen = backend.seen();
    assert!(
        !seen.is_empty()
            && seen
                .iter()
                .all(|seen| seen.as_deref() == Some(authorization.as_bytes())),
        "a cold probe lacked the exact value: {seen:?}"
    );
    let first_metadata = health_check_headers(&first);
    assert_eq!(first_metadata.len(), 1, "{first_metadata:?}");
    assert_eq!(first_metadata[0].name, "Authorization");
    let first_fingerprint = first_metadata[0].fingerprint.to_owned();
    assert!(
        first_fingerprint.starts_with("sozu-hc-hmac-sha256:v1:"),
        "{first_fingerprint}"
    );
    assert_value_absent_everywhere(&first, "");

    let save = first.run(&[
        "state",
        "save",
        "-f",
        saved_path.to_str().expect("utf-8 path"),
    ]);
    outputs.push(output_text(&save));
    assert!(save.status.success(), "{}", outputs[0]);
    let saved =
        String::from_utf8_lossy(&std::fs::read(&saved_path).expect("saved state")).into_owned();
    assert!(
        saved.contains("Authorization") && saved.contains(&first_fingerprint),
        "the saved state lacks the complete header metadata: {saved}"
    );
    assert!(
        !saved.contains(TOKEN) && !saved.contains(&value_path),
        "the saved state holds the value or its source: {saved}"
    );
    let first_log = first.master_log();
    drop(first);

    // Boot two: the saved state is the authority and its epoch is foreign.
    let mark = backend.seen().len();
    let second_extra = format!(
        "saved_state = \"{}\"\n{cluster}",
        saved_path.to_str().expect("utf-8 path")
    );
    let second = Instance::launch(binary, Arc::clone(&temp), "master-2.log", &second_extra);
    let deadline = Instant::now() + VERDICT_BUDGET;
    let (success, missing) = loop {
        let success = total(&health_counters(&second))[0];
        let missing: i64 = worker_counts(&second, &[MISSING_SECRET])
            .values()
            .map(|counts| counts[0])
            .sum();
        if success >= 1 || missing >= 1 {
            break (success, missing);
        }
        assert!(
            Instant::now() < deadline,
            "neither a probe nor a missing secret after the restart:\n{}",
            second.master_log()
        );
        thread::sleep(Duration::from_millis(100));
    };
    assert_eq!(
        success,
        0,
        "the configuration's value was applied over the saved foreign metadata:\n{}",
        second.master_log()
    );
    assert!(missing >= 1, "no probe was refused for a missing secret");
    assert_eq!(
        backend.seen().len(),
        mark,
        "a probe reached the backend before the value was submitted again"
    );
    assert_eq!(
        health_check_headers(&second),
        first_metadata,
        "the saved metadata changed across the restart"
    );

    // The same file through the public CLI replaces the foreign fingerprint
    // with a local one and the probes authenticate.
    let spec = format!("Authorization={value_path}");
    // At the default `info` level: the config's `debug` would have the CLI
    // echo the parsed command, the operator's own argument path included.
    let set = second
        .cli(&[
            "cluster",
            "health-check",
            "set",
            "--id",
            "app",
            "--port",
            port.as_str(),
            "--interval",
            "1",
            "--probe-timeout",
            "1",
            "--healthy-threshold",
            "1",
            "--unhealthy-threshold",
            "1",
            "--header-file",
            spec.as_str(),
        ])
        .env("RUST_LOG", "info")
        .output()
        .expect("run CLI");
    let set_output = output_text(&set);
    assert!(
        set.status.success(),
        "the protected set failed:\n{set_output}"
    );
    let second_metadata = health_check_headers(&second);
    assert_eq!(second_metadata.len(), 1, "{second_metadata:?}");
    assert_eq!(second_metadata[0].name, "Authorization");
    let second_fingerprint = second_metadata[0].fingerprint.as_str();
    assert!(
        second_fingerprint.starts_with("sozu-hc-hmac-sha256:v1:")
            && second_fingerprint != first_fingerprint,
        "{first_fingerprint} -> {second_fingerprint}"
    );
    // The backend was never DOWN: `HealthState::default()` starts Healthy and
    // `record_success` reports a transition, which `health_check.up` counts,
    // only from Unhealthy. UP is therefore not expected here; an authenticated
    // HTTP 200 with `success >= 1`, `failure == 0` and `down == 0` proves the
    // current healthy authenticated probe.
    let sample = wait_for_counters(&second, "a 200 probe", |sample| total(sample)[0] >= 1);
    let [_, failure, down, _] = total(&sample);
    assert_eq!((failure, down), (0, 0), "{sample:?}");
    let probes = backend.seen().split_off(mark);
    assert!(
        !probes.is_empty()
            && probes
                .iter()
                .all(|seen| seen.as_deref() == Some(authorization.as_bytes())),
        "a probe since the value was submitted again lacked it: {probes:?}"
    );
    assert_value_absent_everywhere(&second, &set_output);

    // Neither the value nor the path of its source is on any surface; the
    // only file that holds the value is the harness's, and the only one that
    // names its path is the configuration the harness wrote.
    outputs.push(set_output);
    outputs.push(output_text(&second.run(&[
        "cluster",
        "health-check",
        "list",
    ])));
    outputs.push(output_text(&second.run(&[
        "--json",
        "cluster",
        "health-check",
        "list",
    ])));
    outputs.push(first_log);
    outputs.push(second.master_log());
    for text in &outputs {
        assert!(!text.contains(TOKEN), "a surface shows the value: {text}");
        assert!(
            !text.contains(&value_path),
            "a surface shows the value's source: {text}"
        );
    }
    assert_eq!(
        files_holding(temp.path(), TOKEN.as_bytes()),
        [PathBuf::from("authorization.value")]
    );
    assert_eq!(
        files_holding(temp.path(), value_path.as_bytes()),
        [PathBuf::from("config.toml")]
    );
}
