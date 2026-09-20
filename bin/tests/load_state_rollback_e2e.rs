//! sozu#1313 end-to-end coverage: the state replay, against a real master and
//! a real worker.
//!
//! Three cases, one per half of the incident:
//!
//! 1. [`a_state_file_larger_than_the_command_buffer_loads_completely`] — the
//!    operator's actual shape. A state file many times larger than the worker
//!    channel ceiling (`max_command_buffer_size`) must load COMPLETELY: every
//!    entry reaches the worker, `sozu state load` answers Ok inside a bounded
//!    deadline, the saved state still holds every frontend, and the worker
//!    actually routes the LAST generated hostname — the one past every buffer
//!    refill. Before the fix `load_state` queued every entry onto the worker
//!    inside one event-loop iteration with no flush in between, so past the
//!    ceiling `write_delimited_message` refused frame after frame ("Could not
//!    send request to worker"); those entries were dropped while still being
//!    counted in `expected_responses`, and with `Timeout::None` the task — and
//!    the client — waited forever.
//! 2. [`a_replayed_entry_every_worker_rejects_is_not_re_persisted`] — an entry
//!    the master accepts but every worker refuses must be reverted, so
//!    `SaveState` cannot re-persist it and the next replay cannot re-inject it.
//! 3. [`a_mixed_state_file_keeps_the_accepted_entries_and_reverts_the_rejected_one`]
//!    — the two must coexist in ONE file: the rejected entry is reverted, every
//!    accepted entry survives, and the operator is told how many were reverted.
//!
//! The rejected entry is an `AddHttpFrontend` on an address that has NO HTTP
//! listener. It is well-formed, so it passes the master's pre-dispatch
//! `validate_request` (sozu#1301/#1313 guards) and is committed to
//! `ConfigState`; the worker refuses it with `ProxyError::NoListenerFound`
//! (`HttpProxy::add_http_frontend`, `lib/src/http.rs`) and answers `Failure`. A malformed certificate is
//! also refused by workers, but `compute_rollback` has no unambiguous inverse
//! for `AddCertificate`, so it could not prove anything here.
//!
//! State files are produced with the PRODUCTION serialiser
//! (`ConfigState::write_requests_to_file`) and read back with the PRODUCTION
//! parser (`parse_several_requests`), so the tests exercise exactly the bytes
//! `sozu state save` writes and `sozu state load` replays.
//!
//! These tests need no root, no fixed port and no TTY: a unix socket and every
//! TCP port live in a per-test temp dir / on an ephemeral loopback port. They
//! keep `#[ignore]` so `cargo test` stays fast for contributors, and CI runs
//! them explicitly in its own step (see `.github/workflows/ci.yml`):
//!
//! ```bash
//! cargo test -p sozu --test load_state_rollback_e2e -- --ignored
//! ```

use std::collections::BTreeMap;
use std::fs::File;
use std::io::{Read, Write};
use std::net::{SocketAddr, TcpStream};
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Output, Stdio};
use std::time::{Duration, Instant};

use sozu_command_lib::parser::parse_several_requests;
use sozu_command_lib::proto::command::{
    AddBackend, Cluster, PathRule, PathRuleKind, RequestHttpFrontend, RulePosition, SocketAddress,
    WorkerRequest, request::RequestType,
};
use sozu_command_lib::state::ConfigState;

/// A perfectly well-formed hostname on an address with no listener: the master
/// must ACCEPT this entry (that is the point — the rollback, not the
/// pre-dispatch validation, is under test) and every worker must refuse it.
const REJECTED_HOSTNAME: &str = "no-listener.example.com";

/// Upper bound on every `sozu` CLI invocation these tests make. The master's own
/// deadline for a replay is `worker_timeout` (5 s below) plus 10 ms per entry,
/// and the CLI is allowed 30 s client-side, so a master that hangs — the second
/// half of sozu#1313 — shows up as a hang rather than as a fast client-side
/// timeout. Enforced by [`Instance::run`]: a genuine hang must FAIL the test,
/// never block CI forever.
const CLI_DEADLINE: Duration = Duration::from_secs(25);

/// Worker channel ceiling used by the large-replay test. The production default
/// is 2 MB (`DEFAULT_MAX_COMMAND_BUFFER_SIZE`); shrinking it to 16 KiB
/// reproduces the very same overflow with a state file that serialises and
/// replays in well under a second.
const SMALL_MAX_COMMAND_BUFFER: u64 = 16_384;

/// How many clusters the large replay generates. Each contributes three
/// entries (`AddCluster`, `AddBackend`, `AddHttpFrontend`), so the file is
/// worth many times [`SMALL_MAX_COMMAND_BUFFER`] — asserted, not assumed.
const LARGE_REPLAY_CLUSTERS: usize = 200;

fn sozu_bin() -> &'static str {
    env!("CARGO_BIN_EXE_sozu")
}

/// Grab a currently-free 127.0.0.1 TCP port by binding to `:0` and releasing
/// it. Racy in principle, fine for these tests: nothing else on a CI runner is
/// handing out the same ephemeral port in the same millisecond.
fn free_port() -> u16 {
    std::net::TcpListener::bind("127.0.0.1:0")
        .expect("bind ephemeral port")
        .local_addr()
        .expect("local_addr")
        .port()
}

/// The spawned master, reaped in `drop`.
///
/// Every exit from a test — a clean return, a failed assertion, a `panic!`
/// inside a helper — unwinds through this guard, so the process is killed
/// instead of being leaked into the CI runner. Manual `kill`/`wait` calls at
/// each panic site would only cover the paths someone remembered.
struct MasterProcess(Child);

impl Drop for MasterProcess {
    fn drop(&mut self) {
        let _ = self.0.kill();
        let _ = self.0.wait();
    }
}

/// A running `sozu` master with its own temp dir, config, command socket and
/// captured log.
struct Instance {
    temp: tempfile::TempDir,
    config_arg: String,
    log_path: PathBuf,
    _master: MasterProcess,
}

impl Instance {
    /// Start a master whose worker channels are capped at `max_command_buffer`
    /// and which declares `listener` (when given) as its only HTTP listener.
    /// Returns once a worker has answered `status`: with zero live workers a
    /// fan-out expects nothing, and an entry nobody was asked about is never
    /// rolled back (`should_rollback_fanout` requires `expected > 0`), so a
    /// test could pass for the wrong reason.
    fn start(max_command_buffer: u64, listener: Option<SocketAddr>) -> Self {
        let temp = tempfile::tempdir().expect("tempdir");
        let socket_path = temp.path().join("sozu.sock");
        let config_path = temp.path().join("config.toml");
        let log_path = temp.path().join("master.log");

        let mut config = format!(
            r#"
command_socket = "{socket}"
command_buffer_size = 4096
max_command_buffer_size = {max_command_buffer}
worker_count = 1
worker_automatic_restart = false
worker_timeout = 5
ctl_command_timeout = 30000
log_level = "warn"
log_target = "stdout"
max_connections = 100
buffer_size = 16393
"#,
            socket = socket_path.display(),
        );
        if let Some(address) = listener {
            config.push_str(&format!(
                "\n[[listeners]]\nprotocol = \"http\"\naddress = \"{address}\"\n"
            ));
        }
        std::fs::write(&config_path, &config).expect("write config");

        // The master's own output is captured so a test can assert on it — the
        // large-replay case checks that no entry was refused by the channel —
        // and so a passing CI run stays quiet.
        let log = File::create(&log_path).expect("create master log");
        let log_err = log.try_clone().expect("clone master log handle");
        let master = MasterProcess(
            Command::new(sozu_bin())
                .args(["start", "-c", config_path.to_str().unwrap()])
                .stdout(Stdio::from(log))
                .stderr(Stdio::from(log_err))
                .spawn()
                .expect("spawn sozu start"),
        );

        let instance = Self {
            temp,
            config_arg: config_path.to_str().expect("utf-8 config path").to_owned(),
            log_path,
            _master: master,
        };

        let deadline = Instant::now() + Duration::from_secs(10);
        while !socket_path.exists() {
            if Instant::now() > deadline {
                panic!(
                    "sozu master never created {socket_path:?}\nmaster log:\n{}",
                    instance.master_log()
                );
            }
            std::thread::sleep(Duration::from_millis(50));
        }

        let deadline = Instant::now() + Duration::from_secs(10);
        loop {
            let status = instance.run(&["status"]);
            if status.status.success() {
                break;
            }
            if Instant::now() > deadline {
                panic!(
                    "no worker answered `status` in time:\nstdout=\n{}\nstderr=\n{}\nmaster log:\n{}",
                    String::from_utf8_lossy(&status.stdout),
                    String::from_utf8_lossy(&status.stderr),
                    instance.master_log(),
                );
            }
            std::thread::sleep(Duration::from_millis(100));
        }

        instance
    }

    fn path(&self, name: &str) -> PathBuf {
        self.temp.path().join(name)
    }

    fn master_log(&self) -> String {
        String::from_utf8_lossy(&std::fs::read(&self.log_path).unwrap_or_default()).into_owned()
    }

    /// Run one `sozu` CLI invocation against this instance under a hard
    /// deadline: on expiry the CLI child is killed and the test fails (the
    /// master is reaped by [`MasterProcess`] as the panic unwinds).
    ///
    /// Output is piped and read once the child has exited. Every invocation
    /// here prints a handful of lines, well under the pipe capacity, so polling
    /// without draining cannot deadlock the child.
    fn run(&self, args: &[&str]) -> Output {
        let mut child = Command::new(sozu_bin())
            .args(["-c", &self.config_arg])
            .args(args)
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .unwrap_or_else(|e| panic!("spawn `sozu {}`: {e}", args.join(" ")));
        let started_at = Instant::now();
        loop {
            match child.try_wait().expect("try_wait") {
                Some(_) => return child.wait_with_output().expect("wait_with_output"),
                None => {
                    if started_at.elapsed() > CLI_DEADLINE {
                        let _ = child.kill();
                        let _ = child.wait();
                        panic!(
                            "`sozu {}` did not answer within {CLI_DEADLINE:?} — the master is \
                             hung (sozu#1313)\nmaster log:\n{}",
                            args.join(" "),
                            self.master_log(),
                        );
                    }
                    std::thread::sleep(Duration::from_millis(50));
                }
            }
        }
    }

    /// `sozu state save` into a temp file, read back with the PRODUCTION parser
    /// and replayed into a fresh `ConfigState` — the exact round trip a later
    /// `sozu state load` would perform on this file.
    fn saved_state(&self) -> (ConfigState, String) {
        let saved_path = self.path("out.state");
        let save = self.run(&["state", "save", "-f", saved_path.to_str().unwrap()]);
        assert!(
            save.status.success(),
            "`state save` must succeed.\nstdout=\n{}\nstderr=\n{}",
            String::from_utf8_lossy(&save.stdout),
            String::from_utf8_lossy(&save.stderr),
        );
        let bytes = std::fs::read(&saved_path).expect("read the saved state");
        let (rest, requests) =
            parse_several_requests::<WorkerRequest>(&bytes).expect("the saved state must parse");
        assert!(
            rest.is_empty(),
            "the production parser must consume the whole saved state, {} bytes left",
            rest.len()
        );
        let mut state = ConfigState::new();
        for request in requests {
            state
                .dispatch(&request.content)
                .expect("a saved request must replay into a fresh state");
        }
        (state, String::from_utf8_lossy(&bytes).into_owned())
    }
}

/// A minimal `Tree`-positioned HTTP frontend, valid in every respect.
fn frontend(cluster_id: &str, hostname: &str, address: SocketAddr) -> RequestHttpFrontend {
    RequestHttpFrontend {
        cluster_id: Some(cluster_id.to_owned()),
        address: address.into(),
        hostname: hostname.to_owned(),
        path: PathRule {
            kind: PathRuleKind::Prefix as i32,
            value: "/".to_owned(),
        },
        method: None,
        position: RulePosition::Tree as i32,
        tags: BTreeMap::new(),
        redirect: None,
        redirect_scheme: None,
        redirect_template: None,
        rewrite_host: None,
        rewrite_path: None,
        rewrite_port: None,
        required_auth: None,
        headers: Vec::new(),
        hsts: None,
    }
}

/// Serialise `state` with the production serialiser and return the file size.
fn write_state_file(state: &ConfigState, path: &Path) -> u64 {
    let mut file = File::create(path).expect("create state file");
    state
        .write_requests_to_file(&mut file)
        .expect("serialise the state");
    std::fs::metadata(path).expect("stat the state file").len()
}

/// One HTTP/1.1 request through a worker's listener, returning the status line.
/// `Connection: close` so the read ends on EOF instead of a keep-alive stall.
fn http_status_line(address: SocketAddr, host: &str) -> String {
    let mut stream = TcpStream::connect_timeout(&address, Duration::from_secs(5))
        .unwrap_or_else(|e| panic!("connect to the worker listener at {address}: {e}"));
    stream
        .set_read_timeout(Some(Duration::from_secs(5)))
        .expect("set read timeout");
    write!(
        stream,
        "GET / HTTP/1.1\r\nHost: {host}\r\nConnection: close\r\n\r\n"
    )
    .expect("write the HTTP request");
    let mut response = Vec::new();
    // A timeout is not fatal here: whatever arrived is what we assert on, and
    // an empty response fails the assertion with the same message.
    let _ = stream.read_to_end(&mut response);
    String::from_utf8_lossy(&response)
        .lines()
        .next()
        .unwrap_or_default()
        .to_owned()
}

#[test]
#[ignore = "process-level: spawns a real master and worker; run from the dedicated CI step or with --ignored (see module docs)"]
fn a_state_file_larger_than_the_command_buffer_loads_completely() {
    let listener_address: SocketAddr = format!("127.0.0.1:{}", free_port())
        .parse()
        .expect("listener address");
    let instance = Instance::start(SMALL_MAX_COMMAND_BUFFER, Some(listener_address));
    let state_path = instance.path("large.state");

    // 1) Build the operator's shape: a state file many times the worker channel
    //    ceiling, every entry valid.
    let mut expected_hostnames = Vec::with_capacity(LARGE_REPLAY_CLUSTERS);
    let backend_port = free_port();
    {
        let mut state = ConfigState::new();
        for index in 0..LARGE_REPLAY_CLUSTERS {
            let cluster_id = format!("cluster-{index:04}");
            let hostname = format!("host-{index:04}.example.com");
            state
                .dispatch(
                    &RequestType::AddCluster(Cluster {
                        cluster_id: cluster_id.clone(),
                        ..Default::default()
                    })
                    .into(),
                )
                .expect("ConfigState records the cluster");
            state
                .dispatch(
                    &RequestType::AddBackend(AddBackend {
                        cluster_id: cluster_id.clone(),
                        backend_id: format!("{cluster_id}-0"),
                        address: SocketAddress::new_v4(127, 0, 0, 1, backend_port),
                        ..Default::default()
                    })
                    .into(),
                )
                .expect("ConfigState records the backend");
            state
                .dispatch(
                    &RequestType::AddHttpFrontend(frontend(
                        &cluster_id,
                        &hostname,
                        listener_address,
                    ))
                    .into(),
                )
                .expect("ConfigState records the frontend");
            expected_hostnames.push(hostname);
        }
        let size = write_state_file(&state, &state_path);
        assert!(
            size > SMALL_MAX_COMMAND_BUFFER * 4,
            "the fixture must be comfortably larger than the {SMALL_MAX_COMMAND_BUFFER}-byte \
             channel ceiling this replay has to cross, got {size} bytes"
        );
    }

    // 2) Replay it. Every entry must be delivered, and the client must get an
    //    answer — the two things the incident lost.
    let started_at = Instant::now();
    let load = instance.run(&["state", "load", "-f", state_path.to_str().unwrap()]);
    let load_elapsed = started_at.elapsed();
    let master_log = instance.master_log();

    assert!(
        load_elapsed < CLI_DEADLINE,
        "`state load` must answer within a bounded time, took {load_elapsed:?}"
    );
    assert!(
        !master_log.contains("Could not send request to worker"),
        "no entry may be refused by the worker channel — the overflow is parked and drained, \
         not dropped.\nmaster log:\n{master_log}"
    );
    assert!(
        load.status.success(),
        "a state file of only valid entries must load cleanly.\nstdout=\n{}\nstderr=\n{}\n\
         master log:\n{master_log}",
        String::from_utf8_lossy(&load.stdout),
        String::from_utf8_lossy(&load.stderr),
    );

    // 3) The master kept every entry...
    let (saved, _) = instance.saved_state();
    let saved_hostnames: Vec<String> = saved
        .http_fronts
        .values()
        .map(|front| front.hostname.clone())
        .collect();
    for hostname in &expected_hostnames {
        assert!(
            saved_hostnames.contains(hostname),
            "every replayed frontend must survive in the saved state; {hostname} is missing \
             ({} of {} present)",
            saved_hostnames.len(),
            expected_hostnames.len(),
        );
    }
    assert_eq!(
        saved.count_frontends(),
        LARGE_REPLAY_CLUSTERS,
        "the saved state must hold exactly the replayed frontends"
    );

    // 4) ...and the WORKER actually routes the last one — the entry past every
    //    buffer refill, the first casualty of the dropped-frame bug. A host the
    //    replay never declared is the control: it must still be a 404.
    let last_hostname = expected_hostnames.last().expect("at least one hostname");
    let routed = http_status_line(listener_address, last_hostname);
    let unknown = http_status_line(listener_address, "never-declared.example.com");
    // Both must be real HTTP responses: an empty read would satisfy the
    // `!contains("404")` below without proving anything.
    for (label, status_line) in [("routed", &routed), ("control", &unknown)] {
        assert!(
            status_line.starts_with("HTTP/1.1"),
            "the worker must answer the {label} request with an HTTP response, got \
             {status_line:?}"
        );
    }
    assert!(
        unknown.contains("404"),
        "a host no frontend declares must be a 404, got {unknown:?} — the control assertion is \
         not measuring what it should"
    );
    assert!(
        !routed.contains("404"),
        "the worker must route the LAST replayed hostname (its route table must have received \
         the whole file), got {routed:?} for {last_hostname}\nmaster log:\n{}",
        instance.master_log(),
    );
}

#[test]
#[ignore = "process-level: spawns a real master and worker; run from the dedicated CI step or with --ignored (see module docs)"]
fn a_replayed_entry_every_worker_rejects_is_not_re_persisted() {
    // No listener at all — which is precisely why the worker refuses the
    // frontend below.
    let instance = Instance::start(163_840, None);
    let poisoned_path = instance.path("poisoned.state");
    let orphan_address: SocketAddr = format!("127.0.0.1:{}", free_port())
        .parse()
        .expect("frontend address");

    let mut state = ConfigState::new();
    state
        .dispatch(
            &RequestType::AddHttpFrontend(frontend(
                "rejected-cluster",
                REJECTED_HOSTNAME,
                orphan_address,
            ))
            .into(),
        )
        .expect("ConfigState records a well-formed frontend");
    write_state_file(&state, &poisoned_path);

    // The master commits the entry, the worker refuses it, and THE #1313 FIX
    // reverts that one entry from the master's state.
    let started_at = Instant::now();
    let load = instance.run(&["state", "load", "-f", poisoned_path.to_str().unwrap()]);
    let load_elapsed = started_at.elapsed();
    let load_stdout = String::from_utf8_lossy(&load.stdout).into_owned();
    let load_stderr = String::from_utf8_lossy(&load.stderr).into_owned();

    assert!(
        load_elapsed < CLI_DEADLINE,
        "`state load` must answer within a bounded time, took {load_elapsed:?}"
    );

    // The load-bearing assertion: the entry no worker acknowledged was reverted
    // from the main-process state, so `SaveState` cannot re-persist it and the
    // next replay cannot re-inject it.
    let (saved, saved_text) = instance.saved_state();
    assert!(
        !saved_text.contains(REJECTED_HOSTNAME),
        "an entry every worker rejected must be reverted from the main-process state and never \
         re-persisted by SaveState (sozu#1313); saved state was:\n{saved_text}",
    );
    assert_eq!(
        saved.count_frontends(),
        0,
        "the reverted frontend must be gone from the state, not merely absent from the file"
    );
    // …and the operator is told, instead of getting a silent success.
    assert!(
        !load.status.success(),
        "`state load` must report the worker rejection as a failure.\nstdout=\n{load_stdout}\n\
         stderr=\n{load_stderr}",
    );
}

#[test]
#[ignore = "process-level: spawns a real master and worker; run from the dedicated CI step or with --ignored (see module docs)"]
fn a_mixed_state_file_keeps_the_accepted_entries_and_reverts_the_rejected_one() {
    let listener_address: SocketAddr = format!("127.0.0.1:{}", free_port())
        .parse()
        .expect("listener address");
    let instance = Instance::start(163_840, Some(listener_address));
    let mixed_path = instance.path("mixed.state");
    let orphan_address: SocketAddr = format!("127.0.0.1:{}", free_port())
        .parse()
        .expect("orphan frontend address");

    // Three entries the worker accepts (they sit on the declared listener) and
    // one it cannot (no listener at that address), in ONE file.
    let accepted: Vec<String> = (0..3).map(|i| format!("kept-{i}.example.com")).collect();
    let mut state = ConfigState::new();
    for (index, hostname) in accepted.iter().enumerate() {
        let cluster_id = format!("kept-cluster-{index}");
        state
            .dispatch(
                &RequestType::AddCluster(Cluster {
                    cluster_id: cluster_id.clone(),
                    ..Default::default()
                })
                .into(),
            )
            .expect("ConfigState records the cluster");
        state
            .dispatch(
                &RequestType::AddHttpFrontend(frontend(&cluster_id, hostname, listener_address))
                    .into(),
            )
            .expect("ConfigState records the accepted frontend");
    }
    state
        .dispatch(
            &RequestType::AddHttpFrontend(frontend(
                "rejected-cluster",
                REJECTED_HOSTNAME,
                orphan_address,
            ))
            .into(),
        )
        .expect("ConfigState records the rejected frontend");
    write_state_file(&state, &mixed_path);

    let load = instance.run(&["state", "load", "-f", mixed_path.to_str().unwrap()]);
    let load_output = format!(
        "stdout=\n{}\nstderr=\n{}",
        String::from_utf8_lossy(&load.stdout),
        String::from_utf8_lossy(&load.stderr),
    );

    let (saved, saved_text) = instance.saved_state();
    for hostname in &accepted {
        assert!(
            saved_text.contains(hostname.as_str()),
            "an entry the workers applied must survive the rollback of its neighbour; \
             {hostname} is missing from:\n{saved_text}"
        );
    }
    assert!(
        !saved_text.contains(REJECTED_HOSTNAME),
        "the rejected entry must be reverted, not saved with its accepted neighbours:\n{saved_text}"
    );
    assert_eq!(
        saved.count_frontends(),
        accepted.len(),
        "exactly the accepted frontends must remain"
    );
    assert!(
        !load.status.success(),
        "a replay with a rejected entry must be reported as a failure.\n{load_output}"
    );
    assert!(
        load_output.contains("reverted entries: 1"),
        "the operator must be told how many entries were rolled back.\n{load_output}"
    );
}
