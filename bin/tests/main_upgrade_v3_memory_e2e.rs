//! Real-binary checks of the protocol-3 main-upgrade receiver.
//!
//! The replacement main is the actual `sozu` executable started the way the
//! old main starts it: `main --fd <confirmation> --upgrade-fd <transfer>
//! --upgrade-protocol 3`. A malformed transfer must stop it before it sends
//! `PREPARED`, so the confirmation peer sees end-of-file and no byte.

use std::{
    io::{Read, Write},
    os::{
        fd::AsRawFd,
        unix::{net::UnixStream, process::CommandExt},
    },
    process::{Child, Command, Stdio},
    time::{Duration, Instant},
};

const SOZU: &str = env!("CARGO_BIN_EXE_sozu");
const MAGIC: &[u8; 8] = b"SOZUUPG3";
const FRAME_VERSION: u32 = 1;
const MAX_SNAPSHOT_BYTES: u64 = 1 << 30;
const MAX_PROTECTED_BYTES: u64 = 64 << 20;
const BUDGET_MILLIS: u64 = 5_000;
const WAIT: Duration = Duration::from_secs(20);

fn probe(protocol: &str) -> std::process::ExitStatus {
    Command::new(SOZU)
        .args(["upgrade-probe", "--protocol", protocol])
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()
        .expect("the sozu binary must start")
}

#[test]
fn candidate_accepts_protocols_two_and_three_and_rejects_every_other_value() {
    assert!(probe("2").success(), "protocol 2 must stay supported");
    assert!(probe("3").success(), "protocol 3 must be supported");
    for unknown in ["0", "1", "4", "65535"] {
        assert!(
            !probe(unknown).success(),
            "protocol {unknown} must be rejected"
        );
    }
}

fn header(budget: u64, snapshot_len: u64, protected_len: u64) -> Vec<u8> {
    let mut frame = Vec::new();
    frame.extend_from_slice(MAGIC);
    frame.extend_from_slice(&FRAME_VERSION.to_be_bytes());
    frame.extend_from_slice(&budget.to_be_bytes());
    frame.extend_from_slice(&snapshot_len.to_be_bytes());
    frame.extend_from_slice(&protected_len.to_be_bytes());
    frame
}

struct Outcome {
    success: bool,
    stderr: String,
    confirmation_bytes: usize,
}

fn kill_and_reap(child: &mut Child) {
    let _ = child.kill();
    let _ = child.wait();
}

/// Start the receiver with `frame` on its transfer socket, optionally keep the
/// socket open after the bytes (the sender never half-closes), and report how
/// it ended. The child is owned, bounded and reaped on every path.
fn run_receiver(frame: &[u8], keep_open: bool) -> Outcome {
    let (confirmation_old, confirmation_new) = UnixStream::pair().expect("confirmation pair");
    let (mut sender, receiver) = UnixStream::pair().expect("transfer pair");
    let inherited = [confirmation_new.as_raw_fd(), receiver.as_raw_fd()];
    let mut command = Command::new(SOZU);
    command
        .args([
            "main",
            "--fd",
            &inherited[0].to_string(),
            "--upgrade-fd",
            &inherited[1].to_string(),
            "--upgrade-protocol",
            "3",
        ])
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::piped());
    // SAFETY: `fcntl(F_SETFD)` is async-signal-safe and the descriptors are
    // open in the forked child.
    unsafe {
        command.pre_exec(move || {
            for fd in inherited {
                if libc::fcntl(fd, libc::F_SETFD, 0) < 0 {
                    return Err(std::io::Error::last_os_error());
                }
            }
            Ok(())
        });
    }
    let mut child = command.spawn().expect("the receiver must start");
    drop(confirmation_new);
    drop(receiver);

    sender.write_all(frame).expect("write the frame");
    if !keep_open {
        sender
            .shutdown(std::net::Shutdown::Write)
            .expect("half-close the transfer");
    }

    let started = Instant::now();
    let status = loop {
        if let Some(status) = child.try_wait().expect("poll the receiver") {
            break status;
        }
        if started.elapsed() > WAIT {
            kill_and_reap(&mut child);
            panic!("the receiver did not stop within {WAIT:?}");
        }
        std::thread::yield_now();
    };
    let mut stderr = String::new();
    child
        .stderr
        .take()
        .expect("piped stderr")
        .read_to_string(&mut stderr)
        .expect("read stderr");

    let mut confirmation_old = confirmation_old;
    confirmation_old
        .set_read_timeout(Some(Duration::from_secs(5)))
        .expect("bound the confirmation read");
    let mut seen = Vec::new();
    confirmation_old
        .read_to_end(&mut seen)
        .expect("the confirmation peer must see end-of-file");
    drop(sender);
    Outcome {
        success: status.success(),
        stderr,
        confirmation_bytes: seen.len(),
    }
}

fn assert_refused_before_prepared(outcome: &Outcome, reason: &str) {
    assert!(
        !outcome.success,
        "the receiver must fail: {}",
        outcome.stderr
    );
    assert_eq!(
        outcome.confirmation_bytes, 0,
        "nothing, PREPARED included, may reach the old main: {}",
        outcome.stderr
    );
    assert!(
        outcome.stderr.contains(reason),
        "expected {reason:?} in: {}",
        outcome.stderr
    );
}

#[test]
fn bad_magic_is_refused_before_prepared() {
    let mut frame = header(BUDGET_MILLIS, 2, 0);
    frame[0] ^= 0xff;
    frame.extend_from_slice(b"{}");
    assert_refused_before_prepared(&run_receiver(&frame, false), "unrecognized transfer header");
}

#[test]
fn unknown_frame_version_is_refused_before_prepared() {
    let mut frame = header(BUDGET_MILLIS, 2, 0);
    frame[8..12].copy_from_slice(&2u32.to_be_bytes());
    frame.extend_from_slice(b"{}");
    assert_refused_before_prepared(&run_receiver(&frame, false), "unrecognized transfer header");
}

#[test]
fn oversized_lengths_are_refused_without_reading_a_payload() {
    for (snapshot, protected) in [
        (MAX_SNAPSHOT_BYTES + 1, 0),
        (2, MAX_PROTECTED_BYTES + 1),
        (u64::MAX, 0),
        (0, u64::MAX),
    ] {
        let frame = header(BUDGET_MILLIS, snapshot, protected);
        assert_refused_before_prepared(&run_receiver(&frame, true), "exceeds the maximum");
    }
}

#[test]
fn zero_or_absurd_budget_is_refused() {
    for budget in [0, u64::MAX] {
        let mut frame = header(budget, 2, 0);
        frame.extend_from_slice(b"{}");
        assert_refused_before_prepared(
            &run_receiver(&frame, false),
            "unrecognized transfer header",
        );
    }
}

#[test]
fn truncated_payload_is_refused_before_prepared() {
    let mut frame = header(BUDGET_MILLIS, 100, 0);
    frame.extend_from_slice(b"{\"protocol\":");
    assert_refused_before_prepared(&run_receiver(&frame, false), "truncated");
}

#[test]
fn trailing_bytes_after_the_declared_payload_are_refused() {
    let mut frame = header(BUDGET_MILLIS, 2, 0);
    frame.extend_from_slice(b"{}x");
    assert_refused_before_prepared(&run_receiver(&frame, false), "trailing");
}

#[test]
fn malformed_snapshot_json_is_refused_before_prepared() {
    let mut frame = header(BUDGET_MILLIS, 8, 0);
    frame.extend_from_slice(b"not json");
    assert_refused_before_prepared(&run_receiver(&frame, false), "malformed upgrade snapshot");
}

#[test]
fn a_sender_that_never_finishes_is_bounded_by_the_budget() {
    let mut frame = header(1, 100, 0);
    frame.extend_from_slice(b"{");
    assert_refused_before_prepared(&run_receiver(&frame, true), "deadline");
}
