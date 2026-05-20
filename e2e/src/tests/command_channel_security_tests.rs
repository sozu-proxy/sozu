//! End-to-end regression tests for the command-channel hardening
//! against malformed IPC framing.
//!
//! Two pre-fix bugs in `sozu-command-lib` panicked the receiver on
//! attacker-controlled input — both reachable across the master / worker
//! trust boundary, and one of them externally reachable through the
//! admin unix socket:
//!
//! 1. `Channel::try_read_delimited_message` (`command/src/channel.rs`)
//!    — `&buffer[delimiter_size()..message_len]` panicked when the peer
//!    declared `message_len < delimiter_size()` (e.g. 5 on a 64-bit
//!    host). One malformed packet would crash the master command loop.
//!
//! 2. `ScmSocket::receive_listeners` (`command/src/scm_socket.rs`)
//!    — `received_fds[index..index + len]` panicked when a peer-supplied
//!    `ListenersCount` declared more entries than `MAX_FDS_OUT` (200)
//!    or more than the FDs that actually arrived.
//!
//! These tests reproduce the panic and prove the fix by driving the
//! production code paths (a live worker thread for #1; the real
//! `Server::try_new_from_config` constructor for #2).

use std::{
    io::{IoSlice, Write},
    os::unix::io::{AsRawFd, IntoRawFd},
    thread,
    time::Duration,
};

use mio::net::UnixStream;
use nix::sys::socket::{MsgFlags, sendmsg};
use prost::Message;
use sozu_command_lib::{
    channel::Channel,
    proto::command::{InitialState, ListenersCount, ServerConfig, WorkerRequest, WorkerResponse},
    scm_socket::{ScmSocket, ScmSocketError},
};
use sozu_lib::server::{Server, ServerError};

use crate::sozu::worker::Worker;

/// Reproduces the `channel.rs` slice-OOB DoS: a peer that writes a
/// length delimiter declaring `message_len < delimiter_size()` used to
/// panic the worker on `&buffer[delimiter_size()..message_len]`.
///
/// Post-fix: the worker rejects the frame with
/// `MessageLengthUnderDelimiter`, drops the bogus delimiter from the
/// front buffer so the channel can re-sync, and continues processing
/// the next valid `WorkerRequest`. A follow-up `SoftStop` therefore
/// reaches the worker, exits the event loop cleanly, and
/// `wait_for_server_stop()` returns `true` without the thread having
/// panicked.
///
/// Pre-fix, this test would either:
/// - panic the worker thread on the bogus delimiter and
///   `wait_for_server_stop()` would observe a join error, or
/// - hang forever because the bad bytes were never consumed and the
///   follow-up `SoftStop` was never decoded.
#[test]
fn channel_short_delimiter_does_not_panic_worker() {
    let (config, listeners, state) = Worker::empty_config();
    let mut worker = Worker::start_new_worker("CHANNEL-PANIC-REPRO", config, &listeners, state);

    // Craft a delimiter that declares `message_len = 5`. On a 64-bit
    // host `delimiter_size() == 8`, so the receiver — pre-fix — would
    // panic on `&buffer[8..5]` (slice-end-before-start).
    let bogus_len: usize = 5;
    let bogus = bogus_len.to_le_bytes();
    Write::write_all(&mut worker.command_channel.sock, &bogus)
        .expect("raw write of the bogus delimiter must succeed at the syscall layer");

    // Give the worker a moment to read + reject + consume the bad frame.
    // The fix drops `delimiter_size()` bytes from the front buffer
    // so the channel resyncs on the next inbound frame.
    thread::sleep(Duration::from_millis(200));

    assert!(
        !worker.server_job.is_finished(),
        "worker thread terminated after a bogus 5-byte length prefix — \
         the slice-OOB hardening in command/src/channel.rs has regressed",
    );

    // Re-sync test: send a real SoftStop. If the bad delimiter was
    // dropped correctly, the worker will decode this cleanly and exit
    // its event loop; otherwise it would loop forever on the stale
    // bogus header.
    worker.soft_stop();

    assert!(
        worker.wait_for_server_stop(),
        "worker did not exit cleanly after a SoftStop following the bogus frame — \
         the channel did not re-sync after rejecting MessageLengthUnderDelimiter",
    );
}

/// Reproduces the `scm_socket.rs` slice-OOB DoS at the
/// `Server::try_new_from_config` boundary. The constructor calls
/// `ScmSocket::receive_listeners()` synchronously; pre-fix, a peer
/// `ListenersCount` declaring more http/tls/tcp entries than the FDs
/// actually shipped would panic the worker on
/// `received_fds[index..index + len]`.
///
/// Post-fix, the constructor surfaces the error cleanly as
/// `ServerError::ScmSocket { scm_err: ListenersCountInconsistent, .. }`
/// instead of unwinding from an indexing panic deep in the FD-passing
/// path.
#[test]
fn scm_inconsistent_listeners_count_does_not_panic_server_constructor() {
    // Build the master ↔ worker socket pairs by hand so we can ship a
    // forged manifest before the Server constructor runs.
    let (sender_unix, receiver_unix) =
        UnixStream::pair().expect("could not create scm unix stream pair");

    // The Server constructor expects the SCM fd to outlive the mio
    // UnixStream that opened it; clear FD_CLOEXEC so the helper-spawned
    // path does not invalidate the descriptor at the next `fork+execve`
    // checkpoint. Mirrors `sozu::worker::set_no_close_exec`.
    unsafe { clear_fd_cloexec(sender_unix.as_raw_fd()) };
    unsafe { clear_fd_cloexec(receiver_unix.as_raw_fd()) };

    let sender_fd = sender_unix.into_raw_fd();
    let receiver_fd = receiver_unix.into_raw_fd();

    let sender = ScmSocket::new(sender_fd).expect("master-side scm socket");
    let receiver = ScmSocket::new(receiver_fd).expect("worker-side scm socket");

    // Forge a manifest that declares three HTTP listeners and zero
    // FDs. Pre-fix, the receiver would zip the three addresses against
    // `received_fds[0..3]` — but only 0 FDs arrived so the slice would
    // run past `file_descriptor_length` and panic. Post-fix, the
    // length check fires before any indexing.
    let forged = ListenersCount {
        http: vec![
            "127.0.0.1:80".to_string(),
            "127.0.0.2:80".to_string(),
            "127.0.0.3:80".to_string(),
        ],
        tls: vec![],
        tcp: vec![],
    };
    let payload = forged.encode_length_delimited_to_vec();
    // Bypass `ScmSocket::send_listeners` (which keeps addresses + FDs
    // in sync by construction) and call the underlying `sendmsg`
    // directly so we can ship zero FDs alongside a non-empty manifest.
    send_raw_scm_payload(&sender, &payload).expect("raw scm send must succeed");

    // Build the matching command-channel pair so the Server constructor
    // has a `ProxyChannel` to register. `Channel::<Tx, Rx>::generate`
    // returns `(Channel<Tx, Rx>, Channel<Rx, Tx>)`; the worker side
    // matches `ProxyChannel = Channel<WorkerResponse, WorkerRequest>`,
    // so we instantiate the generator with the *main* side's parameters
    // and consume the second tuple slot. The constructor does not read
    // anything from this channel before the SCM step, which is where
    // the panic used to fire.
    let (_main_side_cmd, worker_side_cmd) =
        Channel::<WorkerRequest, WorkerResponse>::generate(1024, 16384)
            .expect("command channel pair");

    let config = ServerConfig::default();
    let result = Server::try_new_from_config(
        worker_side_cmd,
        receiver,
        config,
        InitialState::default(),
        false,
    );

    match result {
        Err(ServerError::ScmSocket {
            scm_err:
                ScmSocketError::ListenersCountInconsistent {
                    http,
                    tls,
                    tcp,
                    total,
                    fds_received,
                    max_fds: _,
                },
            ..
        }) => {
            assert_eq!(http, 3);
            assert_eq!(tls, 0);
            assert_eq!(tcp, 0);
            assert_eq!(total, 3);
            assert_eq!(fds_received, 0);
        }
        Err(other) => {
            panic!("expected ServerError::ScmSocket(ListenersCountInconsistent), got {other:?}",)
        }
        Ok(_) => panic!(
            "Server::try_new_from_config accepted a forged ListenersCount with no FDs — \
             the SCM bounds check in command/src/scm_socket.rs has regressed",
        ),
    }
}

/// Drop FD_CLOEXEC on `fd`. SAFETY: `fd` must be a valid open file
/// descriptor borrowed for the duration of this call.
unsafe fn clear_fd_cloexec(fd: i32) {
    // SAFETY: `fd` is the caller's invariant; `fcntl` with `F_GETFD` /
    // `F_SETFD` is thread-safe and side-effect-free beyond the
    // descriptor's own flag word.
    unsafe {
        let f = libc::fcntl(fd, libc::F_GETFD);
        libc::fcntl(fd, libc::F_SETFD, f & !libc::FD_CLOEXEC);
    }
}

/// Mirrors the private `ScmSocket::send_msg_and_fds` helper but always
/// ships zero file descriptors — the malformed scenario we want to
/// reproduce. We invoke the `sendmsg` syscall directly to keep the
/// test independent of crate-private helpers.
fn send_raw_scm_payload(socket: &ScmSocket, payload: &[u8]) -> Result<(), std::io::Error> {
    let iov = [IoSlice::new(payload)];
    sendmsg::<()>(socket.fd, &iov, &[], MsgFlags::empty(), None)
        .map(|_| ())
        .map_err(|e| std::io::Error::other(e.to_string()))
}
