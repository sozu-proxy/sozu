//! `sozu cluster health-check` and custom probe headers, against a scripted
//! command peer: `set` offers no raw header option and sends no header unless
//! `--header-file` names its source, a value is read only after the main
//! process confirmed the protected protocol, and no listing prints a header
//! value.
//!
//! Every credential here is a synthetic sentinel.

use std::{
    collections::BTreeMap,
    fs,
    io::{PipeReader, Read, Write},
    os::unix::net::UnixListener,
    path::{Path, PathBuf},
    process::{Command, Output, Stdio},
    sync::{
        Arc,
        atomic::{AtomicBool, Ordering},
        mpsc,
    },
    thread,
    time::{Duration, Instant},
};

use mio::net::UnixStream;
use sozu_command_lib::{
    channel::{Channel, ChannelError},
    proto::command::{
        Cluster, ClusterInformation, ClusterInformations, HealthCheckConfig, HealthCheckHeader,
        HealthCheckHeaderValue, HealthChecksList, ProtectedHealthCheckPhase,
        ProtectedHealthCheckReply, Request, Response, ResponseContent, ResponseStatus,
        WorkerResponses, request::RequestType, response_content::ContentType,
    },
    secret::{FingerprintKey, SecretValue},
};

const SECRET: &str = "SYNTHETIC-HC-LIST-SECRET-3b7f";

/// Accept the CLI's connection, or `None` once the CLI has exited without
/// connecting (a refused command never reaches the peer).
fn accept(listener: &UnixListener, cli_exited: &AtomicBool) -> Option<Channel<Response, Request>> {
    let deadline = Instant::now() + Duration::from_secs(10);
    loop {
        // Read before accepting: a CLI that connected and then exited left its
        // connection queued, and must not be mistaken for one that never came.
        let exited = cli_exited.load(Ordering::SeqCst);
        match listener.accept() {
            Ok((stream, _)) => {
                let mut channel = Channel::new(UnixStream::from_std(stream), 16_384, 163_840);
                channel.blocking().expect("blocking command peer");
                return Some(channel);
            }
            Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => {
                if exited {
                    return None;
                }
                assert!(Instant::now() < deadline, "CLI did not connect");
                thread::sleep(Duration::from_millis(5));
            }
            Err(error) => panic!("accept command peer: {error}"),
        }
    }
}

/// Bind the command socket of a CLI run under `dir` and write the
/// configuration that points the CLI at it; return the listener and the
/// configuration path.
fn command_peer(dir: &Path) -> (UnixListener, PathBuf) {
    let socket = dir.join("command.sock");
    let config = dir.join("config.toml");
    fs::write(
        &config,
        format!(
            "command_socket = {:?}\ncommand_buffer_size = 16384\nmax_command_buffer_size = 163840\nlog_target = \"stdout\"\n",
            socket.to_str().expect("socket path")
        ),
    )
    .expect("config");
    let listener = UnixListener::bind(&socket).expect("command listener");
    listener.set_nonblocking(true).expect("nonblocking accept");
    (listener, config)
}

/// Run `sozu <args>` against a peer that answers its one request with `reply`;
/// return the request the CLI sent and its output.
///
/// `refused` is true for a command the CLI must reject locally: the peer then
/// sees no connection or an EOF before any request, and that is the only
/// outcome it tolerates. Any other peer error or panic fails the test.
fn run(
    args: &[&str],
    reply: Option<ResponseContent>,
    refused: bool,
) -> (Option<RequestType>, Output) {
    let dir = tempfile::tempdir().expect("tempdir");
    let (listener, config) = command_peer(dir.path());
    let cli_exited = Arc::new(AtomicBool::new(false));
    let peer_exited = Arc::clone(&cli_exited);
    let peer = thread::spawn(move || {
        let mut channel = accept(&listener, &peer_exited)?;
        let request = match channel.read_message_blocking_timeout(Some(Duration::from_secs(10))) {
            Ok(message) => message.request_type,
            Err(ChannelError::NoByteToRead) if refused => return None,
            Err(error) => panic!("CLI request: {error:?}"),
        };
        assert!(!refused, "a refused command must not send a request");
        channel
            .write_message(&Response::new(ResponseStatus::Ok, "ok".into(), reply))
            .expect("reply");
        request
    });
    let mut argv = vec!["-c", config.to_str().expect("config path"), "-t", "2000"];
    argv.extend_from_slice(args);
    let output = Command::new(env!("CARGO_BIN_EXE_sozu"))
        .args(&argv)
        .output()
        .expect("run CLI");
    cli_exited.store(true, Ordering::SeqCst);
    let request = peer.join().expect("command peer thread panicked");
    (request, output)
}

/// Raw argv output of a refused `sozu <args>`: the CLI must neither connect
/// nor echo the sentinel.
fn assert_refused_locally(args: &[&str], sentinel: &str) {
    let (request, output) = run(args, None, true);
    assert!(!output.status.success(), "{args:?} must be refused");
    assert!(
        request.is_none(),
        "a refused command must not send a request"
    );
    let text = format!(
        "{}{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    assert!(!text.contains(sentinel), "output echoed the value: {text}");
}

/// The raw `--header name:value` surface is gone: clap refuses the option in
/// both of its spellings before any connection, and nothing it prints echoes
/// the value, which would otherwise sit in argv and in the shell history.
#[test]
fn set_has_no_raw_header_option() {
    let sentinel = "SYNTHETIC-HC-ARG-SECRET-c40e";
    let separate = format!("Authorization: {sentinel}");
    let joined = format!("--header=Authorization: {sentinel}");
    for tail in [vec!["--header", separate.as_str()], vec![joined.as_str()]] {
        let mut args = vec!["cluster", "health-check", "set", "--id", "app"];
        args.extend(tail);
        assert_refused_locally(&args, sentinel);
    }
}

/// `set --help` offers `--header-file`, which names a source and never takes a
/// value, and no raw header option that would put a credential on the command
/// line. The match is on whole option tokens: `--header-file` contains
/// `--header` as a prefix and must not satisfy the second check.
#[test]
fn set_help_offers_header_file_and_no_raw_header_option() {
    let (_, output) = run(&["cluster", "health-check", "set", "--help"], None, true);
    let text = String::from_utf8_lossy(&output.stdout);
    assert!(text.contains("--uri"), "the help was not printed: {text}");
    let tokens: Vec<&str> = text
        .split(|c: char| c.is_whitespace() || "[]<>=,()'\"".contains(c))
        .collect();
    assert!(
        tokens.contains(&"--header-file"),
        "the help does not offer --header-file: {text}"
    );
    assert!(
        !tokens.contains(&"--header"),
        "the help offers --header: {text}"
    );
}

/// A `set` sends a configuration without header metadata: with no way to give
/// the values, the CLI configures no header.
#[test]
fn set_sends_a_configuration_without_headers() {
    let (request, output) = run(
        &["cluster", "health-check", "set", "--id", "app"],
        None,
        false,
    );
    assert!(output.status.success(), "{output:?}");
    let Some(RequestType::SetHealthCheck(set)) = request else {
        panic!("expected SetHealthCheck, got {request:?}");
    };
    assert!(set.config.headers.is_empty());
}

/// The fingerprint the main process would publish for `SECRET`.
fn fingerprint_of_secret() -> String {
    let key = FingerprintKey::generate().expect("the OS random generator is available");
    let mut carrier = HealthCheckHeaderValue {
        name: "Authorization".to_owned(),
        value: SECRET.as_bytes().to_vec(),
    };
    let value = SecretValue::take_from_ingress(&mut carrier).expect("a valid value");
    key.fingerprint("Authorization", &value).to_string()
}

fn config_with_metadata(fingerprint: &str) -> HealthCheckConfig {
    HealthCheckConfig {
        uri: "/".to_owned(),
        interval: 10,
        timeout: 5,
        healthy_threshold: 3,
        unhealthy_threshold: 3,
        headers: vec![HealthCheckHeader {
            name: "Authorization".to_owned(),
            fingerprint: fingerprint.to_owned(),
        }],
        ..Default::default()
    }
}

fn cluster_info(fingerprint: &str) -> ClusterInformations {
    ClusterInformations {
        vec: vec![ClusterInformation {
            configuration: Some(Cluster {
                cluster_id: "app".to_owned(),
                health_check: Some(config_with_metadata(fingerprint)),
                ..Default::default()
            }),
            ..Default::default()
        }],
    }
}

/// The three response shapes that reach a health-check configuration: the
/// listing, a cluster query, and worker responses nesting either.
fn replies(fingerprint: &str) -> Vec<(&'static str, ResponseContent)> {
    let content = |content_type| ResponseContent {
        content_type: Some(content_type),
    };
    let listing = || {
        content(ContentType::HealthChecksList(HealthChecksList {
            map: BTreeMap::from([("app".to_owned(), config_with_metadata(fingerprint))]),
        }))
    };
    let clusters = || content(ContentType::Clusters(cluster_info(fingerprint)));
    let workers = |inner| {
        content(ContentType::WorkerResponses(WorkerResponses {
            map: BTreeMap::from([("0".to_owned(), inner)]),
        }))
    };
    vec![
        ("health check list", listing()),
        ("cluster list", clusters()),
        ("worker-nested health check list", workers(listing())),
        ("worker-nested cluster list", workers(clusters())),
    ]
}

#[test]
fn listings_never_print_a_header_value_in_human_or_json_output() {
    let fingerprint = fingerprint_of_secret();
    for (shape, reply) in replies(&fingerprint) {
        for json in [false, true] {
            let mut args = vec![];
            if json {
                args.push("--json");
            }
            args.extend(["cluster", "health-check", "list"]);
            let (_, output) = run(&args, Some(reply.to_owned()), false);
            assert!(output.status.success(), "{shape}: {output:?}");
            let stdout = String::from_utf8_lossy(&output.stdout);
            let stderr = String::from_utf8_lossy(&output.stderr);
            assert!(
                !stdout.contains(SECRET) && !stderr.contains(SECRET),
                "{shape} (json={json}) leaked a header value: {stdout}{stderr}"
            );
            // The cluster table does not render a health check; the health-check
            // table and every JSON shape show the name and the fingerprint.
            if json || shape.ends_with("health check list") {
                assert!(
                    stdout.contains("Authorization") && stdout.contains(&fingerprint),
                    "{shape} (json={json}) shows the name and the fingerprint: {stdout}"
                );
            }
        }
    }
}

// --- `set --header-file`: a value is read only once the main process confirmed
// the protected protocol. ---

/// How the scripted peer answers one request.
enum Answer {
    /// One response with this status and content.
    Reply(ResponseStatus, Option<ResponseContent>),
    /// Two progress reports, then the response.
    ProgressThenReply(ResponseStatus, Option<ResponseContent>),
    /// No answer at all.
    Silent,
    /// A progress report every 50 ms for 12 seconds, then the channel closes.
    ProgressForever,
    /// Close the channel without answering.
    Close,
}

/// The standard input of a CLI run, owned by the test. The pipe's read end is
/// shared with the CLI, so a byte the CLI never read is still in the pipe
/// afterwards.
struct OwnedStdin {
    child_end: Option<PipeReader>,
    probe: PipeReader,
    /// Releases the held-open write end, which a watchdog closes by itself
    /// after a minute so that a CLI that wrongly reads blocks no test run.
    release: Option<(mpsc::Sender<()>, thread::JoinHandle<()>)>,
}

impl OwnedStdin {
    /// `bytes` pending in a pipe whose write end stays open: a read past them
    /// blocks instead of ending.
    fn held_open(bytes: &[u8]) -> Self {
        let (reader, mut writer) = std::io::pipe().expect("stdin pipe");
        writer.write_all(bytes).expect("pending stdin");
        let (release, released) = mpsc::channel();
        let watchdog = thread::spawn(move || {
            let _ = released.recv_timeout(Duration::from_secs(60));
            drop(writer);
        });
        Self {
            probe: reader.try_clone().expect("probe the pipe"),
            child_end: Some(reader),
            release: Some((release, watchdog)),
        }
    }

    /// `bytes` followed by the end of the input.
    fn closed(bytes: &[u8]) -> Self {
        let (reader, mut writer) = std::io::pipe().expect("stdin pipe");
        writer.write_all(bytes).expect("stdin bytes");
        drop(writer);
        Self {
            probe: reader.try_clone().expect("probe the pipe"),
            child_end: Some(reader),
            release: None,
        }
    }

    /// What the CLI left in the pipe.
    fn unread(mut self) -> Vec<u8> {
        if let Some((release, watchdog)) = self.release.take() {
            release.send(()).expect("release the write end");
            watchdog.join().expect("watchdog");
        }
        let mut left = Vec::new();
        self.probe
            .read_to_end(&mut left)
            .expect("read the pipe back");
        left
    }
}

struct Outcome {
    /// Whether the CLI opened a connection to the command socket.
    connected: bool,
    requests: Vec<RequestType>,
    output: Output,
    elapsed: Duration,
    /// What the CLI left unread of its owned standard input, if it had one.
    stdin_left: Option<Vec<u8>>,
}

impl Outcome {
    /// The verbs the CLI sent, in order.
    fn verbs(&self) -> Vec<&'static str> {
        self.requests
            .iter()
            .map(|request| match request {
                RequestType::SetHealthCheck(_) => "set",
                RequestType::QueryProtectedHealthCheckSupport(_) => "capability",
                RequestType::SetProtectedHealthCheck(_) => "protected-set",
                _ => "other",
            })
            .collect()
    }

    fn text(&self) -> String {
        format!(
            "{}{}",
            String::from_utf8_lossy(&self.output.stdout),
            String::from_utf8_lossy(&self.output.stderr)
        )
    }

    /// Nothing the CLI printed or the peer captured shows one of `values`.
    fn assert_no_value_shown(&self, values: &[&str]) {
        let text = self.text();
        let captured = format!("{:?}", self.requests);
        for value in values {
            assert!(!text.contains(value), "the output shows a value: {text}");
            assert!(
                !captured.contains(value),
                "a captured request shows a value: {captured}"
            );
        }
    }
}

/// Serve one CLI connection, answering its requests with `answers` in order
/// (a request beyond them is answered with a failure). Returns whether the CLI
/// connected, and every request it sent.
fn serve(
    listener: &UnixListener,
    cli_exited: &AtomicBool,
    answers: Vec<Answer>,
) -> (bool, Vec<RequestType>) {
    let Some(mut channel) = accept(listener, cli_exited) else {
        return (false, vec![]);
    };
    let give_up = Instant::now() + Duration::from_secs(30);
    let mut answers = answers.into_iter();
    let mut requests = vec![];
    // A write fails once the CLI gave up and closed the channel; what the CLI
    // did is asserted from the outcome, not from the peer.
    let respond = |channel: &mut Channel<Response, Request>, status, content| {
        let _ = channel.write_message(&Response::new(status, "scripted".into(), content));
    };
    loop {
        match channel.read_message_blocking_timeout(Some(Duration::from_millis(200))) {
            Ok(request) => {
                requests.extend(request.request_type);
                match answers
                    .next()
                    .unwrap_or(Answer::Reply(ResponseStatus::Failure, None))
                {
                    Answer::Reply(status, content) => respond(&mut channel, status, content),
                    Answer::ProgressThenReply(status, content) => {
                        respond(&mut channel, ResponseStatus::Processing, None);
                        respond(&mut channel, ResponseStatus::Processing, None);
                        respond(&mut channel, status, content);
                    }
                    Answer::Silent => {}
                    Answer::ProgressForever => {
                        let end = Instant::now() + Duration::from_secs(12);
                        while !cli_exited.load(Ordering::SeqCst) && Instant::now() < end {
                            if channel
                                .write_message(&Response::new(
                                    ResponseStatus::Processing,
                                    "still working".into(),
                                    None,
                                ))
                                .is_err()
                            {
                                break;
                            }
                            thread::sleep(Duration::from_millis(50));
                        }
                        break;
                    }
                    Answer::Close => break,
                }
            }
            Err(ChannelError::NoByteToRead) => break,
            Err(ChannelError::TimeoutReached(_)) => {
                if cli_exited.load(Ordering::SeqCst) {
                    break;
                }
                assert!(Instant::now() < give_up, "the CLI neither sent nor closed");
            }
            Err(error) => panic!("CLI request: {error:?}"),
        }
    }
    (true, requests)
}

/// Run `sozu -t <global_timeout_ms> <args>` against a peer that answers with
/// `answers`, with `stdin` as the CLI's standard input (closed otherwise).
fn scenario(
    global_timeout_ms: u64,
    args: &[&str],
    answers: Vec<Answer>,
    stdin: Option<OwnedStdin>,
) -> Outcome {
    let dir = tempfile::tempdir().expect("tempdir");
    let (listener, config) = command_peer(dir.path());
    let cli_exited = Arc::new(AtomicBool::new(false));
    let peer_exited = Arc::clone(&cli_exited);
    let peer = thread::spawn(move || serve(&listener, &peer_exited, answers));
    let timeout = global_timeout_ms.to_string();
    let mut command = Command::new(env!("CARGO_BIN_EXE_sozu"));
    command
        .args(["-c", config.to_str().expect("config path"), "-t", &timeout])
        .args(args);
    let mut stdin = stdin;
    match stdin.as_mut() {
        Some(owned) => command.stdin(Stdio::from(owned.child_end.take().expect("one run"))),
        None => command.stdin(Stdio::null()),
    };
    let started = Instant::now();
    let output = command.output().expect("run CLI");
    let elapsed = started.elapsed();
    cli_exited.store(true, Ordering::SeqCst);
    let (connected, requests) = peer.join().expect("command peer thread panicked");
    Outcome {
        connected,
        requests,
        output,
        elapsed,
        stdin_left: stdin.map(OwnedStdin::unread),
    }
}

const SET: [&str; 5] = ["cluster", "health-check", "set", "--id", "app"];

fn with_set<'a>(tail: &[&'a str]) -> Vec<&'a str> {
    let mut args = SET.to_vec();
    args.extend_from_slice(tail);
    args
}

fn capability_reply(phase: ProtectedHealthCheckPhase, cluster_id: &str, version: u64) -> Answer {
    Answer::Reply(
        ResponseStatus::Ok,
        Some(ResponseContent {
            content_type: Some(ContentType::ProtectedHealthCheckReply(
                ProtectedHealthCheckReply {
                    phase: phase as i32,
                    cluster_id: cluster_id.to_owned(),
                    version,
                },
            )),
        }),
    )
}

/// The one answer that confirms support.
fn confirmed() -> Answer {
    capability_reply(
        ProtectedHealthCheckPhase::ProtectedHealthCheckCapability,
        "",
        0,
    )
}

const PENDING: &[u8] = b"SYNTHETIC-HC-STDIN-PENDING-5a1c\n";
const OPEN_FAILURE: &str = "could not open the health check header file";

/// `set` without `--header-file` is the ordinary `SetHealthCheck`: no header
/// and no capability query.
#[test]
fn headerless_set_sends_only_set_health_check() {
    let outcome = scenario(
        2000,
        &SET,
        vec![Answer::Reply(ResponseStatus::Ok, None)],
        None,
    );
    assert!(outcome.output.status.success(), "{:?}", outcome.output);
    assert_eq!(outcome.verbs(), ["set"]);
    let Some(RequestType::SetHealthCheck(set)) = outcome.requests.first() else {
        panic!("expected SetHealthCheck, got {:?}", outcome.requests);
    };
    assert!(set.config.headers.is_empty());
}

/// Silence from the main process, with the global command timeout disabled,
/// ends at the capability query's own five-second budget. Nothing was read:
/// the absent path was never opened and the pending standard input is intact.
#[test]
fn a_silent_peer_fails_at_the_capability_deadline_even_with_timeout_zero() {
    let dir = tempfile::tempdir().expect("tempdir");
    let absent = format!("X-Absent={}", dir.path().join("absent").display());
    let outcome = scenario(
        0,
        &with_set(&["--header-file", &absent, "--header-file", "Authorization=-"]),
        vec![Answer::Silent],
        Some(OwnedStdin::held_open(PENDING)),
    );
    assert!(!outcome.output.status.success(), "{:?}", outcome.output);
    assert!(
        outcome.elapsed >= Duration::from_secs(5) && outcome.elapsed < Duration::from_secs(8),
        "the deadline is five seconds, not {:?}",
        outcome.elapsed
    );
    assert_eq!(outcome.verbs(), ["capability"]);
    assert!(
        !outcome.text().contains(OPEN_FAILURE),
        "the absent path was opened: {}",
        outcome.text()
    );
    assert_eq!(outcome.stdin_left.as_deref(), Some(PENDING));
}

/// Progress reports are not an answer: a peer that never stops sending them
/// cannot stretch the five-second budget.
#[test]
fn progress_replies_cannot_renew_the_capability_deadline() {
    let outcome = scenario(
        0,
        &with_set(&["--header-file", "Authorization=-"]),
        vec![Answer::ProgressForever],
        Some(OwnedStdin::held_open(PENDING)),
    );
    assert!(!outcome.output.status.success(), "{:?}", outcome.output);
    assert!(
        outcome.elapsed >= Duration::from_secs(5) && outcome.elapsed < Duration::from_secs(8),
        "the deadline is five seconds, not {:?}",
        outcome.elapsed
    );
    assert_eq!(outcome.verbs(), ["capability"]);
    assert_eq!(outcome.stdin_left.as_deref(), Some(PENDING));
}

/// Anything but the exact typed positive answer is a refusal, and a refusal
/// reads no input and sends no value.
#[test]
fn an_unconfirmed_capability_reads_no_input_and_sends_no_value() {
    use ProtectedHealthCheckPhase::{ProtectedHealthCheckCapability, ProtectedHealthCheckPrepare};
    let other_content = Some(ResponseContent {
        content_type: Some(ContentType::HealthChecksList(HealthChecksList::default())),
    });
    let cases: Vec<(&str, Answer)> = vec![
        ("generic Ok", Answer::Reply(ResponseStatus::Ok, None)),
        (
            "Ok with other content",
            Answer::Reply(ResponseStatus::Ok, other_content),
        ),
        (
            "explicit refusal",
            Answer::Reply(ResponseStatus::Failure, None),
        ),
        (
            "wrong phase",
            capability_reply(ProtectedHealthCheckPrepare, "", 0),
        ),
        (
            "wrong cluster",
            capability_reply(ProtectedHealthCheckCapability, "app", 0),
        ),
        (
            "wrong version",
            capability_reply(ProtectedHealthCheckCapability, "", 1),
        ),
        (
            "exact payload under a failure status",
            Answer::Reply(
                ResponseStatus::Failure,
                match confirmed() {
                    Answer::Reply(_, content) => content,
                    _ => unreachable!("confirmed() is a reply"),
                },
            ),
        ),
        ("closed channel", Answer::Close),
    ];
    for (label, answer) in cases {
        let dir = tempfile::tempdir().expect("tempdir");
        let absent = format!("X-Absent={}", dir.path().join("absent").display());
        let outcome = scenario(
            2000,
            &with_set(&["--header-file", &absent, "--header-file", "Authorization=-"]),
            vec![answer],
            Some(OwnedStdin::held_open(PENDING)),
        );
        assert!(
            !outcome.output.status.success(),
            "{label}: {:?}",
            outcome.output
        );
        assert_eq!(outcome.verbs(), ["capability"], "{label}");
        assert!(
            !outcome.text().contains(OPEN_FAILURE),
            "{label}: the absent path was opened: {}",
            outcome.text()
        );
        assert_eq!(outcome.stdin_left.as_deref(), Some(PENDING), "{label}");
    }
}

/// The same absent path that a refusal never opened is opened after the exact
/// typed positive answer, even one that follows progress reports: the capability
/// answer is the boundary.
#[test]
fn a_confirmed_capability_reaches_the_file_open() {
    let dir = tempfile::tempdir().expect("tempdir");
    let absent = format!("X-Absent={}", dir.path().join("absent").display());
    let positive = match confirmed() {
        Answer::Reply(status, content) => Answer::ProgressThenReply(status, content),
        _ => unreachable!("confirmed() is a reply"),
    };
    let outcome = scenario(
        2000,
        &with_set(&["--header-file", &absent]),
        vec![positive],
        None,
    );
    assert!(!outcome.output.status.success(), "{:?}", outcome.output);
    assert!(
        outcome.text().contains(OPEN_FAILURE),
        "the absent path was not opened: {}",
        outcome.text()
    );
    assert_eq!(outcome.verbs(), ["capability"]);
}

const FILE_SECRET: &str = "SYNTHETIC-HC-FILE-SECRET-91d2";
const STDIN_SECRET: &str = "SYNTHETIC-HC-STDIN-SECRET-2e77";

/// After the exact typed positive answer, files and standard input become one
/// `SetProtectedHealthCheck`: names in declared order with no fingerprint, one
/// carrier per name in the same order, and the bytes exactly as read except one
/// terminal LF and the CR before it. An empty input is a value.
#[test]
fn values_are_sent_in_declared_order_with_only_the_final_line_ending_removed() {
    let dir = tempfile::tempdir().expect("tempdir");
    let file = |name: &str, bytes: &[u8]| {
        let path = dir.path().join(name);
        fs::write(&path, bytes).expect("value file");
        format!("{name}={}", path.display())
    };
    let specs = [
        file("X-Lf", format!("{FILE_SECRET}\n").as_bytes()),
        "Authorization=-".to_owned(),
        file("X-Empty", b""),
        file("X-Crlf", b"bravo\r\n"),
        file("X-Bare-Lf", b"\n"),
        file("X-Bare-Crlf", b"\r\n"),
        file("X-Inner", "gamma delta\tepsilon \u{e9}".as_bytes()),
    ];
    let mut tail = vec![];
    for spec in &specs {
        tail.extend(["--header-file", spec.as_str()]);
    }
    let outcome = scenario(
        2000,
        &with_set(&tail),
        vec![confirmed(), Answer::Reply(ResponseStatus::Ok, None)],
        Some(OwnedStdin::closed(format!("{STDIN_SECRET}\r\n").as_bytes())),
    );
    assert!(outcome.output.status.success(), "{:?}", outcome.output);
    assert_eq!(outcome.verbs(), ["capability", "protected-set"]);
    let Some(RequestType::SetProtectedHealthCheck(set)) = outcome.requests.get(1) else {
        panic!(
            "expected SetProtectedHealthCheck, got {:?}",
            outcome.requests
        );
    };
    let expected: [(&str, &[u8]); 7] = [
        ("X-Lf", FILE_SECRET.as_bytes()),
        ("Authorization", STDIN_SECRET.as_bytes()),
        ("X-Empty", b""),
        ("X-Crlf", b"bravo"),
        ("X-Bare-Lf", b""),
        ("X-Bare-Crlf", b""),
        ("X-Inner", "gamma delta\tepsilon \u{e9}".as_bytes()),
    ];
    assert_eq!(set.cluster_id, "app");
    assert_eq!(
        set.config
            .headers
            .iter()
            .map(|header| (header.name.as_str(), header.fingerprint.as_str()))
            .collect::<Vec<_>>(),
        expected
            .iter()
            .map(|(name, _)| (*name, ""))
            .collect::<Vec<_>>(),
        "metadata: declared order, no fingerprint"
    );
    assert_eq!(
        set.values
            .iter()
            .map(|carrier| (carrier.name.as_str(), carrier.value.as_slice()))
            .collect::<Vec<_>>(),
        expected,
        "carriers: same order, exact bytes"
    );
    assert_eq!((set.config.uri.as_str(), set.config.interval), ("/", 10));
    assert_eq!(outcome.stdin_left.as_deref(), Some(&b""[..]));
    outcome.assert_no_value_shown(&[FILE_SECRET, STDIN_SECRET]);
}

/// Bytes that remain after the one terminal line ending are not trimmed or
/// repaired: each is refused by the header value rules, so no value is sent.
/// Were the repeated LF, the internal CR, the spaces or the tab stripped, the
/// request would go out.
#[test]
fn bytes_that_remain_after_the_final_line_ending_are_refused_not_trimmed() {
    const SPACES: &str = "begin or end with a space or a tab";
    // (label, bytes before the sentinel, bytes after it, the refusal)
    let cases: [(&str, &str, &str, &str); 8] = [
        ("repeated LF", "", "\n\n", "control bytes"),
        ("internal LF", "ab\n", "\n", "control bytes"),
        ("lone CR", "", "\r", "control bytes"),
        ("CR before CRLF", "", "\r\r\n", "control bytes"),
        ("repeated CRLF", "", "\r\n\r\n", "control bytes"),
        ("leading space", " ", "\n", SPACES),
        ("trailing space", "", " \n", SPACES),
        ("trailing tab", "", "\t", SPACES),
    ];
    for (label, before, after, reason) in cases {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("value");
        fs::write(&path, format!("{before}{FILE_SECRET}{after}")).expect("value file");
        let spec = format!("Authorization={}", path.display());
        let outcome = scenario(
            2000,
            &with_set(&["--header-file", &spec]),
            vec![confirmed()],
            None,
        );
        assert!(
            !outcome.output.status.success(),
            "{label}: {:?}",
            outcome.output
        );
        assert_eq!(outcome.verbs(), ["capability"], "{label}: no value is sent");
        assert!(
            outcome.text().contains(reason),
            "{label}: the value rules did not refuse it: {}",
            outcome.text()
        );
        outcome.assert_no_value_shown(&[FILE_SECRET]);
    }
}

/// A specification the CLI can refuse by itself is refused before it connects,
/// opens a path or reads standard input, with the reason of the existing
/// health-check rules.
#[test]
fn a_bad_header_file_specification_is_refused_before_any_connection_or_input() {
    let many: Vec<String> = (0..33)
        .map(|index| format!("X-Header-{index}=absent"))
        .collect();
    let mut thirty_three = vec![];
    for spec in &many {
        thirty_three.extend(["--header-file", spec.as_str()]);
    }
    let cases: Vec<(&str, Vec<&str>, &str)> = vec![
        (
            "no separator",
            vec!["--header-file", "Authorization"],
            "NAME=PATH",
        ),
        (
            "empty name",
            vec!["--header-file", "=absent"],
            "RFC 9110 token",
        ),
        (
            "empty path",
            vec!["--header-file", "Authorization="],
            "NAME=PATH",
        ),
        (
            "name that is not a token",
            vec!["--header-file", "Bad Name=absent"],
            "RFC 9110 token",
        ),
        (
            "reserved name",
            vec!["--header-file", "Host=absent"],
            "reserved",
        ),
        (
            "duplicate name, compared case-insensitively",
            vec![
                "--header-file",
                "Authorization=absent",
                "--header-file",
                "authorization=absent",
            ],
            "unique",
        ),
        (
            "TCP mode",
            vec!["--mode", "tcp", "--header-file", "Authorization=absent"],
            "HTTP mode only",
        ),
        ("more than 32 headers", thirty_three, "more than 32"),
        (
            "standard input twice",
            vec!["--header-file", "A-One=-", "--header-file", "A-Two=-"],
            "standard input",
        ),
    ];
    for (label, tail, reason) in cases {
        let outcome = scenario(
            2000,
            &with_set(&tail),
            vec![confirmed()],
            Some(OwnedStdin::held_open(PENDING)),
        );
        assert!(
            !outcome.output.status.success(),
            "{label}: {:?}",
            outcome.output
        );
        assert!(!outcome.connected, "{label}: the CLI connected");
        assert!(outcome.requests.is_empty(), "{label}: a request was sent");
        assert!(
            outcome.text().contains(reason),
            "{label}: expected the reason {reason:?}, got: {}",
            outcome.text()
        );
        assert_eq!(outcome.stdin_left.as_deref(), Some(PENDING), "{label}");
    }
}
