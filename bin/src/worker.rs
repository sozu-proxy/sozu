#[cfg(target_os = "freebsd")]
use std::{ffi::c_void, iter::repeat, mem::size_of};
use std::{
    fs::File,
    io::{Error as IoError, Seek},
    net::SocketAddr,
    os::unix::{
        io::{AsRawFd, FromRawFd, IntoRawFd},
        process::CommandExt,
    },
    process::Command,
};

use libc::pid_t;
use mio::net::UnixStream;
use nix::{
    errno::Errno,
    sys::signal::{SigHandler, Signal, signal},
    unistd::{ForkResult, fork},
};
use sozu_command_lib::{
    channel::{Channel, ChannelError},
    config::{Config, MetricDetailLevel},
    logging::{AccessLogFormat, LOGGER, LogError, setup_logging},
    proto::command::{MetricDetail, ServerConfig, WorkerRequest, WorkerResponse},
    ready::Ready,
    request::{RequestError, read_initial_state_from_file},
    scm_socket::{Listeners, ScmSocket, ScmSocketError},
    state::{ConfigState, StateError},
};
use sozu_lib::{
    metrics::{self, MetricError},
    server::{Server, ServerError as LibServerError},
};
use tempfile::tempfile;

use crate::util::{self, UtilError};

#[derive(thiserror::Error, Debug)]
pub enum WorkerError {
    #[error("could not ignore SIGTERM: {0}")]
    IgnoreSigterm(Errno),
    #[error("could not read on the channel")]
    ReadChannel(ChannelError),
    #[error("could not parse configuration from temporary file: {0}")]
    ReadRequestsFromFile(RequestError),
    #[error("could not setup metrics on new worker: {0}")]
    SetupMetrics(MetricError),
    #[error("could not create new worker from config: {0}")]
    NewServerFromConfig(LibServerError),
    #[error("could not create {kind} scm socket: {scm_err}")]
    CreateScmSocket {
        kind: String,
        scm_err: ScmSocketError,
    },
    #[error("could not create temporary file to pass the state to the new worker: {0}")]
    CreateStateFile(IoError),
    #[error("could not disable cloexec on {fd_name}'s file descriptor: {util_err}")]
    DisableCloexec {
        fd_name: String,
        util_err: UtilError,
    },
    #[error("could not write state to temporary file: {0}")]
    WriteStateFile(StateError),
    #[error("could not rewind the temporary state file: {0}")]
    Rewind(IoError),
    #[error("could not create MIO pair of unix stream: {0}")]
    CreateUnixStream(IoError),
    #[error("could not send config to the new worker: {0}")]
    SendConfig(ChannelError),
    #[error("could not send listeners to the new worker: {0}")]
    SendListeners(ScmSocketError),
    #[error("unix fork failed: {0}")]
    Fork(Errno),
    #[error("Could not set the worker-to-main channel to {state}: {channel_err}")]
    SetChannel {
        state: String,
        channel_err: ChannelError,
    },
    #[error("could not setup the logger: {0}")]
    SetupLogging(LogError),
    /// `exec` of the worker failed in the forked child. The child logs this
    /// error as its last line and ends; it never returns it, because that
    /// process is a copy of the main process (see [`fork_main_into_worker`]).
    #[error("could not spawn child process: {0}")]
    SpawnChild(std::io::Error),
}

/// Called within a worker process, this starts the actual proxy.
/// Only called from the binary entry point (main.rs), not from the library.
#[allow(dead_code)]
pub fn begin_worker_process(
    worker_to_main_channel_fd: i32,
    worker_to_main_scm_fd: i32,
    configuration_state_fd: i32,
    id: i32,
    command_buffer_size: u64,
    max_command_buffer_size: u64,
) -> Result<(), WorkerError> {
    // The three descriptors were handed to us by the master via
    // `fork_main_into_worker`, each derived from a live `as_raw_fd()`, so
    // they are always valid (>= 0) and pairwise distinct — three separate
    // kernel objects (command channel, SCM socket, state file). A negative
    // value or a collision would mean the master's fd bookkeeping aliased
    // two handoffs, an internal logic bug. (Stays a `debug_assert!`: a
    // genuinely bad descriptor surfaces downstream as a returned channel /
    // file error, never a panic.)
    debug_assert!(
        worker_to_main_channel_fd >= 0,
        "inherited worker-to-main channel fd must be a valid descriptor"
    );
    debug_assert!(
        worker_to_main_scm_fd >= 0,
        "inherited worker-to-main SCM fd must be a valid descriptor"
    );
    debug_assert!(
        configuration_state_fd >= 0,
        "inherited configuration-state fd must be a valid descriptor"
    );
    debug_assert!(
        worker_to_main_channel_fd != worker_to_main_scm_fd
            && worker_to_main_channel_fd != configuration_state_fd
            && worker_to_main_scm_fd != configuration_state_fd,
        "the channel, SCM and state descriptors must be three distinct fds, never aliased"
    );

    // A unit without `ExecStop=` stops with SIGTERM to its whole control
    // group. The main process turns its own SIGTERM into a soft stop of the
    // workers (`CommandHub::handle_sigterm`); a worker killed by the same
    // signal would die with its log buffers unflushed. If the main process dies
    // instead, the worker still leaves: its channel closes and `Server::run`
    // returns.
    // SAFETY: `SigIgn` installs no handler code.
    unsafe { signal(Signal::SIGTERM, SigHandler::SigIgn) }.map_err(WorkerError::IgnoreSigterm)?;

    let mut worker_to_main_channel: Channel<WorkerResponse, ServerConfig> = Channel::new(
        // SAFETY: `worker_to_main_channel_fd` was just inherited from the
        // pre-exec parent process via the `--fd` CLI argument. It is a valid
        // open descriptor with no other owner inside this freshly-execed
        // worker process. Ownership transfers to the `UnixStream`, whose
        // `Drop` closes the descriptor.
        unsafe { UnixStream::from_raw_fd(worker_to_main_channel_fd) },
        command_buffer_size,
        max_command_buffer_size,
    );

    worker_to_main_channel
        .blocking()
        .map_err(|channel_err| WorkerError::SetChannel {
            state: "blocking".to_string(),
            channel_err,
        })?;

    // SAFETY: `configuration_state_fd` was just inherited from the
    // pre-exec parent process via the `--configuration-state-fd` CLI
    // argument. It is a valid open descriptor with no other owner inside
    // this freshly-execed worker process. Ownership transfers to the
    // `File`, whose `Drop` closes the descriptor.
    let mut configuration_state_file = unsafe { File::from_raw_fd(configuration_state_fd) };

    let worker_config = worker_to_main_channel
        .read_message()
        .map_err(WorkerError::ReadChannel)?;

    let worker_id = format!("{}-{:02}", "WRK", id);

    // The display id is derived purely from the numeric `id` the master
    // assigned (`WorkerId`, a monotonic counter handed in via `--id`): it
    // must carry the canonical `WRK-` prefix and embed that very id so log
    // correlation between master and worker is unambiguous.
    debug_assert!(
        worker_id.starts_with("WRK-"),
        "worker display id must carry the canonical WRK- prefix"
    );
    debug_assert!(
        id < 0 || worker_id.ends_with(&id.to_string()),
        "worker display id must embed the numeric worker id handed in by the master"
    );

    let access_log_format = AccessLogFormat::from(&worker_config.access_log_format());

    // do not try to log anything before this, or the logger will panic
    setup_logging(
        &worker_config.log_target,
        worker_config.log_colored,
        worker_config.access_logs_target.as_deref(),
        Some(access_log_format),
        Some(worker_config.log_colored),
        &worker_config.log_level,
        &worker_id,
    )
    .map_err(WorkerError::SetupLogging)?;

    trace!(
        "Creating worker {} with config: {:#?}",
        worker_id, worker_config
    );
    info!("worker {} starting...", id);

    let initial_state = read_initial_state_from_file(&mut configuration_state_file)
        .map_err(WorkerError::ReadRequestsFromFile)?;
    // The state file is an unlinked temporary file: close it once read, or
    // the worker keeps its storage alive for its whole life.
    drop(configuration_state_file);

    worker_to_main_channel
        .nonblocking()
        .map_err(|channel_err| WorkerError::SetChannel {
            state: "nonblocking".to_string(),
            channel_err,
        })?;

    let mut worker_to_main_channel: Channel<WorkerResponse, WorkerRequest> =
        worker_to_main_channel.into();
    worker_to_main_channel.readiness.insert(Ready::READABLE);

    if let Some(metrics) = worker_config.metrics.as_ref() {
        let address = metrics
            .address
            .parse::<SocketAddr>()
            .expect("Could not parse metrics address");
        // Convert the proto wire enum back into the configuration enum.
        // Workers receive the detail level over the SCM socket as
        // `Option<i32>`; absent / unrecognised values fall back to the
        // historical default (Cluster) so old binaries on either side keep
        // working.
        let detail = metrics
            .detail
            .and_then(|d| MetricDetail::try_from(d).ok())
            .map(MetricDetailLevel::from)
            .unwrap_or_default();
        metrics::setup(
            &address,
            worker_id,
            metrics.tagged_metrics,
            metrics.prefix.clone(),
            detail,
        )
        .map_err(WorkerError::SetupMetrics)?;
    }

    let worker_to_main_scm_socket =
        ScmSocket::new(worker_to_main_scm_fd).map_err(|scm_err| WorkerError::CreateScmSocket {
            kind: "worker-to-main".to_string(),
            scm_err,
        })?;

    let mut server = Server::try_new_from_config(
        worker_to_main_channel,
        worker_to_main_scm_socket,
        worker_config,
        initial_state,
        true,
    )
    .map_err(WorkerError::NewServerFromConfig)?;

    info!("starting event loop");
    server.run();
    info!("ending event loop");
    // Drain the log buffers while `server`, and with it the channel to the
    // main process, is still alive. The main process answers that channel
    // closing with `SIGKILL` (`Server::close_worker`), and the channel closes
    // when `server` drops at the end of this function — before the
    // thread-local `LOGGER` would drop and flush on its own. A `file://`
    // target keeps up to a buffer's worth of records back, and they used to
    // die with the worker on `SoftStop`, `HardStop` and worker upgrade.
    LOGGER.with(|logger| logger.borrow_mut().flush());
    Ok(())
}

/// The listening sockets a worker upgrade hands to the main process, owned by
/// [`fork_main_into_worker`] for the length of one launch.
///
/// [`Listeners`] holds raw descriptors and has no `Drop`. The new worker gets
/// its own copies over the SCM socket, so the main process keeps none of them
/// on any outcome: closing them on drop covers the success path and every
/// error return alike. An error return that left them open would keep the main
/// process listening on sockets it never accepts on, close-on-exec so that no
/// worker inherits them either, until it exits.
struct OwnedListeners(Listeners);

impl Drop for OwnedListeners {
    fn drop(&mut self) {
        self.0.close();
    }
}

/// The main end of a new worker's SCM socket, owned by
/// [`fork_main_into_worker`] until its launch has succeeded.
///
/// [`ScmSocket`] borrows its raw descriptor and has no `Drop`, so an error
/// return between creating the socket and handing it to the caller would leave
/// the descriptor open in the main process for good. The failure repeats on
/// every automatic restart until `EMFILE`, after which the main process can
/// neither accept a client nor spawn a worker.
struct OwnedScmSocket(ScmSocket);

impl OwnedScmSocket {
    /// Hand the socket, and the duty to close it, to the caller.
    fn release(self) -> ScmSocket {
        // `ManuallyDrop` keeps `Drop::drop` from closing the descriptor the
        // returned copy now stands for.
        let released = std::mem::ManuallyDrop::new(self);
        released.0.clone()
    }
}

impl std::ops::Deref for OwnedScmSocket {
    type Target = ScmSocket;

    fn deref(&self) -> &ScmSocket {
        &self.0
    }
}

impl Drop for OwnedScmSocket {
    fn drop(&mut self) {
        // SAFETY: `fork_main_into_worker` is the sole owner of this
        // descriptor until `release`, which `drop` never follows. It created
        // the socket from an owned stream's raw descriptor and handed it
        // nowhere else, so nothing else closes it.
        if unsafe { libc::close(self.0.raw_fd()) } != 0 {
            error!(
                "could not close the SCM socket of a worker that failed to launch: {}",
                IoError::last_os_error()
            );
        }
    }
}

/// A forked worker that has not reached `CommandServer::register_worker`.
///
/// No event-loop session owns this PID yet. Every error between `fork` and the
/// successful return must therefore kill and reap it here; dropping the
/// channels alone makes the child exit but leaves a zombie with no registered
/// PID for the regular reaper to find.
struct UnregisteredWorker(Option<pid_t>);

impl UnregisteredWorker {
    fn release(mut self) -> pid_t {
        self.0
            .take()
            .expect("an unregistered worker is released at most once")
    }
}

impl Drop for UnregisteredWorker {
    fn drop(&mut self) {
        if let Some(pid) = self.0.take() {
            util::kill_and_reap_child(pid, "replacement worker");
        }
    }
}

/// unix-forks the main process
///
/// - Parent: sends config, state and listeners to the new worker
/// - Child: calls the sozu executable path like so: `sozu worker --id <worker_id> [...]`.
///   If that `exec` fails, the child logs the error and ends with `_exit(1)`
///   instead of returning: it is a copy of the whole main process, and
///   returning would resume that copy inside the caller's event loop. The
///   parent then sees the worker's channel close.
///
/// Takes ownership of the descriptors in `listeners`: the main process closes
/// them when this function returns, whether the launch succeeded or not. An
/// error also leaves no descriptor of this launch open in the main process,
/// the SCM socket included.
///
/// returns the child process pid, and channels to talk to it.
pub fn fork_main_into_worker(
    worker_id: &str,
    config: &Config,
    executable_path: String,
    state: &ConfigState,
    listeners: Option<Listeners>,
) -> Result<(pid_t, Channel<WorkerRequest, WorkerResponse>, ScmSocket), WorkerError> {
    // SAFETY: `libc::getpid` takes no input pointers, never fails, and
    // returns a value type. No invariant beyond "FFI signature matches libc".
    trace!("parent({})", unsafe { libc::getpid() });

    // From here on every `?` returns through this binding, which closes the
    // listening sockets: they have no `Drop` of their own.
    let listeners = listeners.map(OwnedListeners);

    let mut state_file = tempfile().map_err(WorkerError::CreateStateFile)?;
    util::disable_close_on_exec(state_file.as_raw_fd()).map_err(|util_err| {
        WorkerError::DisableCloexec {
            fd_name: "state_file".to_string(),
            util_err,
        }
    })?;

    state
        .write_initial_state_to_file(&mut state_file)
        .map_err(WorkerError::WriteStateFile)?;

    state_file.rewind().map_err(WorkerError::Rewind)?;

    let (main_to_worker, worker_to_main) =
        UnixStream::pair().map_err(WorkerError::CreateUnixStream)?;
    let (main_to_worker_scm, worker_to_main_scm) =
        UnixStream::pair().map_err(WorkerError::CreateUnixStream)?;

    // `ScmSocket::new` closes the descriptor itself when it fails; once it
    // succeeded, the guard closes it on every later error return.
    let main_to_worker_scm = OwnedScmSocket(
        ScmSocket::new(main_to_worker_scm.into_raw_fd()).map_err(|scm_err| {
            WorkerError::CreateScmSocket {
                kind: "main-to-worker".to_string(),
                scm_err,
            }
        })?,
    );

    util::disable_close_on_exec(worker_to_main.as_raw_fd()).map_err(|util_err| {
        WorkerError::DisableCloexec {
            fd_name: "worker-to-main".to_string(),
            util_err,
        }
    })?;
    util::disable_close_on_exec(worker_to_main_scm.as_raw_fd()).map_err(|util_err| {
        WorkerError::DisableCloexec {
            fd_name: "worker-to-main-scm".to_string(),
            util_err,
        }
    })?;

    // The three descriptors the worker child will inherit via its CLI args are
    // distinct kernel objects — none handed off twice: the command channel
    // (`worker_to_main`, `--fd`), the SCM socket (`worker_to_main_scm`,
    // `--scm`) and the state file (`--configuration-state-fd`). A collision
    // would alias two roles onto one fd after exec. (The SCM listener fds
    // themselves travel separately over `send_listeners`, not as CLI args.)
    debug_assert!(
        worker_to_main.as_raw_fd() != worker_to_main_scm.as_raw_fd()
            && worker_to_main.as_raw_fd() != state_file.as_raw_fd()
            && worker_to_main_scm.as_raw_fd() != state_file.as_raw_fd(),
        "worker channel, SCM and state descriptors must be three distinct fds, never aliased"
    );

    let worker_config = ServerConfig::from(config);

    let mut main_to_worker_channel: Channel<ServerConfig, WorkerResponse> = Channel::new(
        main_to_worker,
        worker_config.command_buffer_size,
        worker_config.max_command_buffer_size,
    );

    // DISCUSS: should we really block the channel just to write on it?
    if let Err(e) = main_to_worker_channel.blocking() {
        error!("Could not block the main-to-worker channel: {}", e);
    }

    info!("launching worker {}", worker_id);
    debug!("executable path is {}", executable_path);

    // SAFETY: `fork` is unsafe because the child must avoid touching
    // shared mutable state inherited from the parent. The child branch
    // below restricts itself to `Command::exec`, which replaces the process
    // image entirely — no inherited state matters after that point — and,
    // when `exec` fails, to one log line, a log flush and `_exit`.
    match unsafe { fork().map_err(WorkerError::Fork)? } {
        ForkResult::Parent { child: worker_pid } => {
            // A successful `fork()` returns the child's pid in the parent
            // branch, which is always strictly positive (0 is the child
            // branch, < 0 never reaches here — `fork` would have returned
            // `Err`). A non-positive pid here would mean we mis-classified
            // the fork outcome and are about to register a bogus worker.
            debug_assert!(
                worker_pid.as_raw() > 0,
                "fork parent branch must observe a strictly positive child pid"
            );
            let worker_process = UnregisteredWorker(Some(worker_pid.as_raw()));
            info!("launching worker {} with pid {}", worker_id, worker_pid);
            main_to_worker_channel
                .write_message(&worker_config)
                .map_err(WorkerError::SendConfig)?;

            main_to_worker_channel
                .nonblocking()
                .map_err(|channel_err| WorkerError::SetChannel {
                    state: "nonblocking".to_string(),
                    channel_err,
                })?;

            // The new worker holds its own copies once they are sent; the
            // main process closes its own when `listeners` drops, whatever the
            // outcome of the send.
            if let Some(OwnedListeners(listeners)) = &listeners {
                info!("sending listeners to new worker: {:?}", listeners);
                main_to_worker_scm
                    .send_listeners(listeners)
                    .map_err(WorkerError::SendListeners)?;
                info!("sent listeners from main");
            };

            // `main_to_worker_scm` stays close-on-exec: every worker forked
            // later must not inherit the main end of this worker's SCM
            // socket. Only a main upgrade hands it across `exec`, and
            // `CommandHub::disable_cloexec_before_upgrade`
            // (`bin/src/command/server.rs`) clears the flag for that hand-off
            // alone.
            Ok((
                worker_process.release(),
                main_to_worker_channel.into(),
                main_to_worker_scm.release(),
            ))
        }
        ForkResult::Child => {
            // SAFETY: `libc::getpid` takes no input pointers, never fails,
            // and returns a value type. No invariant beyond "FFI signature
            // matches libc".
            trace!("child({}):\twill spawn a child", unsafe { libc::getpid() });

            // #515: prefer fd-based exec (`/proc/self/fd/<n>` opened with
            // `O_PATH` on `/proc/self/exe`) so that a worker spawn after
            // the on-disk binary at `executable_path` was replaced still
            // execs the **original** inode the master was started from —
            // race-free against `cp new-sozu /usr/bin/sozu` between master
            // startup and worker auto-restart. The forked child inherits
            // the parent's mmap'd image, so `/proc/self/exe` in the child
            // resolves to the same original inode.
            let exec_path = match crate::util::get_executable_exec_path() {
                Ok(p) => p,
                Err(e) => {
                    error!(
                        "could not open /proc/self/exe O_PATH ({}); falling back to path-based exec — \
                         vulnerable to on-disk binary replacement race per #515",
                        e
                    );
                    executable_path.to_owned()
                }
            };
            let err = Command::new(&exec_path)
                .arg("worker")
                .arg("--id")
                .arg(worker_id)
                .arg("--fd")
                .arg(worker_to_main.as_raw_fd().to_string())
                .arg("--scm")
                .arg(worker_to_main_scm.as_raw_fd().to_string())
                .arg("--configuration-state-fd")
                .arg(state_file.as_raw_fd().to_string())
                .arg("--command-buffer-size")
                .arg(config.command_buffer_size.to_string())
                .arg("--max-command-buffer-size")
                .arg(config.max_command_buffer_size.to_string())
                .exec();

            // `exec` only returns on failure, and this process is a full copy
            // of the main process: its event loop, its client and worker
            // channels and its poll instance. Returning the error would hand
            // that copy back to the command hub that called us, where it would
            // go on serving the same descriptors beside the real main and,
            // with a worker still missing from its own stale registry, fork
            // another copy of itself. The parent branch reports the missing
            // worker through the closing channel, so this process ends here.
            error!("worker {}: {}", worker_id, WorkerError::SpawnChild(err));

            // Drain the log buffers first: `_exit` skips the thread-local
            // `LOGGER` drop, and a `file://` target would lose this line, the
            // only record of why the spawn failed. The buffer is a copy of the
            // main process's, so on that failure path the records it still
            // held when we forked can appear twice.
            LOGGER.with(|logger| logger.borrow_mut().flush());

            // SAFETY: `_exit` takes no pointer and never returns. Unlike
            // `std::process::exit` it runs none of the `atexit` handlers or
            // destructors of the main process this child is a copy of.
            unsafe { libc::_exit(1) }
        }
    }
}

#[cfg(all(test, target_os = "linux"))]
mod tests {
    use std::time::{Duration, Instant};

    use super::*;

    /// Exit status the test gives a process that came back out of
    /// `fork_main_into_worker` with a `SpawnChild` error, which only the
    /// forked child can do. No other path of the function uses it.
    const CLONE_RETURNED_INTO_CALLER: libc::c_int = 97;

    /// Reap `pid` with a bounded wait; kill it and report `None` on timeout.
    fn wait_for_exit(pid: libc::pid_t, deadline: Duration) -> Option<libc::c_int> {
        let give_up = Instant::now() + deadline;
        let mut status = 0;
        loop {
            // SAFETY: `pid` is a child of this test process and `status` is a
            // valid output slot.
            let waited = unsafe { libc::waitpid(pid, &mut status, libc::WNOHANG) };
            if waited == pid {
                return Some(status);
            }
            if Instant::now() >= give_up {
                // SAFETY: `pid` is a live, unreaped child of this process.
                unsafe {
                    libc::kill(pid, libc::SIGKILL);
                    libc::waitpid(pid, &mut status, 0);
                }
                return None;
            }
            std::thread::sleep(Duration::from_millis(5));
        }
    }

    /// When `exec` fails in the forked worker child, that child is a copy of
    /// the whole main process. It must end there, not return the error into
    /// the caller: the caller is the command hub's event loop, and a returned
    /// copy would go on serving the same descriptors beside the real main.
    ///
    /// A NUL byte in the worker id makes `Command::exec` fail with
    /// `InvalidInput` before any `execve`, whatever the host allows.
    #[test]
    fn failed_exec_ends_the_forked_child_instead_of_returning_into_the_caller() {
        let config = sendable_config();
        let state = ConfigState::new();

        match fork_main_into_worker(
            "worker\0id",
            &config,
            "/nonexistent/sozu".to_string(),
            &state,
            None,
        ) {
            // Only the forked child's `exec` arm yields `SpawnChild`. This
            // process is the clone: report that it survived and leave without
            // running the rest of the harness.
            Err(WorkerError::SpawnChild(_)) => {
                // SAFETY: `_exit` takes no pointer and never returns; the clone
                // must not unwind into the test harness it no longer owns.
                unsafe { libc::_exit(CLONE_RETURNED_INTO_CALLER) }
            }
            Err(other) => panic!("the parent could not set up the spawn: {other}"),
            Ok((pid, _channel, _scm)) => {
                let status = wait_for_exit(pid, Duration::from_secs(10))
                    .expect("the forked child neither exited nor returned within 10 s");
                assert!(
                    libc::WIFEXITED(status),
                    "the forked child must exit by itself, wait status {status:#x}"
                );
                assert_ne!(
                    libc::WEXITSTATUS(status),
                    CLONE_RETURNED_INTO_CALLER,
                    "the forked child returned `Err(SpawnChild)` into its caller and kept \
                     running as a second copy of the main process instead of ending"
                );
            }
        }
    }

    /// Exit status a forked scenario reports when `fork_main_into_worker`
    /// returned `Ok`: the scenario did not break the launch, so the test would
    /// prove nothing.
    const LAUNCH_SUCCEEDED: libc::c_int = 100;
    /// Exit status a forked scenario reports when its body panicked.
    const SCENARIO_PANICKED: libc::c_int = 101;
    /// Exit status of a scenario whose listener descriptor was still open.
    const LISTENER_LEFT_OPEN: libc::c_int = 102;
    /// Exit status of a scenario whose launch failed on another edge than the
    /// one it sets up.
    const WRONG_EDGE: libc::c_int = 103;
    /// Exit status of a scenario whose failed launch left a live child or a
    /// zombie for the event loop, which never registered its PID, to miss.
    const CHILD_LEFT_UNREAPED: libc::c_int = 104;

    /// Run `scenario` in a forked copy of this test process and return its exit
    /// code, or `None` when it did not end within the deadline.
    ///
    /// The scenarios change process-global state (`RLIMIT_NOFILE`) and read the
    /// descriptor table, so none of them may run beside the other tests of this
    /// process. The child is a single-threaded copy: it reaches `_exit` without
    /// unwinding into the harness it no longer owns. It also inherits every
    /// lock another test thread held at the fork, with no owner left to release
    /// it, so a scenario never writes the environment: `set_var` waits there
    /// for good on a reader of the environment lock.
    fn in_forked_child(scenario: impl FnOnce() -> libc::c_int) -> Option<libc::c_int> {
        // SAFETY: `fork` is called with no lock of ours held. The child only
        // runs the scenario, which is the code under test, then `_exit`.
        let pid = unsafe { libc::fork() };
        assert!(pid >= 0, "could not fork the scenario process");
        if pid == 0 {
            // SAFETY: `setpgid` takes no pointer. The group gathers the workers
            // the scenario forks, before it forks any.
            unsafe { libc::setpgid(0, 0) };
            let code = std::panic::catch_unwind(std::panic::AssertUnwindSafe(scenario))
                .unwrap_or(SCENARIO_PANICKED);
            // SAFETY: `_exit` takes no pointer and never returns.
            unsafe { libc::_exit(code) }
        }
        let status = wait_for_exit(pid, Duration::from_secs(30));
        // A worker the scenario forked is waited for by nobody, and can stay
        // stuck for good in its error log on the stdout lock another thread
        // held at the fork: end the whole group. While a member lives, its id
        // cannot be given to another process; with none left this fails.
        // SAFETY: `kill` takes no pointer.
        unsafe { libc::kill(-pid, libc::SIGKILL) };
        let status = status?;
        assert!(
            libc::WIFEXITED(status),
            "the scenario process must exit by itself, wait status {status:#x}"
        );
        Some(libc::WEXITSTATUS(status))
    }

    /// Every descriptor number below 1024 that is open in this process. It only
    /// calls `fcntl`, so taking a snapshot opens nothing itself.
    fn open_descriptors() -> std::collections::BTreeSet<libc::c_int> {
        (0..1024)
            // SAFETY: `F_GETFD` on an arbitrary number reads a flag or fails
            // with `EBADF`; it takes no pointer and touches no descriptor.
            .filter(|fd| unsafe { libc::fcntl(*fd, libc::F_GETFD) } != -1)
            .collect()
    }

    /// A `Config` whose command channel cannot carry its own `ServerConfig`:
    /// `fork_main_into_worker` forks, then fails to send the configuration with
    /// `MessageTooLarge`.
    fn config_that_cannot_be_sent() -> Config {
        Config {
            command_buffer_size: 1,
            max_command_buffer_size: 1,
            ..Default::default()
        }
    }

    fn sendable_config() -> Config {
        Config {
            command_buffer_size: 16384,
            max_command_buffer_size: 163_840,
            ..Default::default()
        }
    }

    /// An HTTP listening socket on an ephemeral port, as the old worker hands
    /// it to the main process in the first phase of a worker upgrade.
    fn received_listeners() -> (Listeners, libc::c_int) {
        let listener = std::net::TcpListener::bind("127.0.0.1:0")
            .expect("could not bind an ephemeral listening socket");
        let address = listener.local_addr().expect("listener has an address");
        let fd = listener.into_raw_fd();
        let listeners = Listeners {
            http: vec![(
                sozu_command_lib::listener_key::ListenerKey::from(address),
                fd,
            )],
            ..Default::default()
        };
        (listeners, fd)
    }

    fn oversized_received_listeners() -> (Listeners, libc::c_int) {
        let mut listeners = Listeners::default();
        let mut first_fd = None;
        for _ in 0..201 {
            let listener = std::net::TcpListener::bind("127.0.0.1:0")
                .expect("could not bind an ephemeral listening socket");
            let address = listener.local_addr().expect("listener has an address");
            let fd = listener.into_raw_fd();
            first_fd.get_or_insert(fd);
            listeners.http.push((address.into(), fd));
        }
        (
            listeners,
            first_fd.expect("the oversized listener set is non-empty"),
        )
    }

    /// Hand `listeners` to a launch that `break_launch` makes fail, and report
    /// whether the listening socket is still open once it returned.
    ///
    /// A NUL in the worker id keeps the forked child from ever reaching
    /// `execve`, as in the test above.
    fn failed_launch_listener_status(
        make_listeners: impl FnOnce() -> (Listeners, libc::c_int),
        config: Config,
        break_launch: impl FnOnce(),
        expected_edge: fn(&WorkerError) -> bool,
    ) -> libc::c_int {
        let (listeners, fd) = make_listeners();
        break_launch();
        let state = ConfigState::new();
        let launched = fork_main_into_worker(
            "worker\0id",
            &config,
            "/nonexistent/sozu".to_string(),
            &state,
            Some(listeners),
        );
        // Nothing may open a descriptor between the failure and this probe: a
        // new descriptor would reuse the listener's number once it is closed.
        // SAFETY: `F_GETFD` reads a flag or fails with `EBADF`.
        let still_open = unsafe { libc::fcntl(fd, libc::F_GETFD) } != -1;
        let mut child_status = 0;
        // SAFETY: this isolated scenario had no child before the launch. A
        // non-negative result means the failed launch left either a live child
        // (`0`) or one zombie that this probe just reaped (`> 0`).
        let child_left = unsafe { libc::waitpid(-1, &mut child_status, libc::WNOHANG) } >= 0;
        match launched {
            Ok(_) => LAUNCH_SUCCEEDED,
            Err(error) if !expected_edge(&error) => WRONG_EDGE,
            Err(_) if still_open => LISTENER_LEFT_OPEN,
            Err(_) if child_left => CHILD_LEFT_UNREAPED,
            Err(_) => 0,
        }
    }

    /// Lower `RLIMIT_NOFILE` so that this process can open at most `room` more
    /// descriptors. The limit binds a privileged process as well, unlike
    /// `RLIMIT_NPROC`.
    fn leave_room_for_descriptors(room: libc::rlim_t) {
        // SAFETY: `dup` and `close` act on descriptors of this single-threaded
        // scenario process; `limit` is a valid slot.
        unsafe {
            let lowest_free = libc::dup(0);
            assert!(lowest_free >= 0, "could not find the lowest free fd");
            libc::close(lowest_free);
            let mut limit = libc::rlimit {
                rlim_cur: 0,
                rlim_max: 0,
            };
            assert_eq!(libc::getrlimit(libc::RLIMIT_NOFILE, &mut limit), 0);
            limit.rlim_cur = lowest_free as libc::rlim_t + room;
            assert_eq!(libc::setrlimit(libc::RLIMIT_NOFILE, &limit), 0);
        }
    }

    fn assert_listeners_closed(edge: &str, status: Option<libc::c_int>) {
        assert_eq!(
            status,
            Some(0),
            "a launch that failed {edge} must close the listening sockets it was handed, or \
             main keeps them listening without ever accepting (scenario status: \
             {LISTENER_LEFT_OPEN} = still open, {LAUNCH_SUCCEEDED} = the launch did not fail, \
             {WRONG_EDGE} = it failed elsewhere, {CHILD_LEFT_UNREAPED} = child was not reaped, \
             {SCENARIO_PANICKED} = panicked)"
        );
    }

    /// The listening sockets a worker upgrade hands to the main process are
    /// raw descriptors with no `Drop`. A launch that fails before the fork must
    /// close them, not leave the main process listening on sockets it never
    /// accepts on: with one worker new connections complete the handshake and
    /// hang, with several `SO_REUSEPORT` still hashes a share of them there.
    /// The descriptor limit leaves no room for the state file.
    #[test]
    fn a_launch_failing_to_create_the_state_file_closes_the_listeners() {
        let status = in_forked_child(|| {
            failed_launch_listener_status(
                received_listeners,
                Config::default(),
                || leave_room_for_descriptors(0),
                |error| matches!(error, WorkerError::CreateStateFile(_)),
            )
        });
        assert_listeners_closed("to create its state file", status);
    }

    /// Same defect on the edge `UnixStream::pair` reports: the descriptor limit
    /// leaves room for the state file and no more; the pair needs two.
    #[test]
    fn a_launch_failing_to_create_the_socket_pairs_closes_the_listeners() {
        let status = in_forked_child(|| {
            failed_launch_listener_status(
                received_listeners,
                Config::default(),
                || leave_room_for_descriptors(1),
                |error| matches!(error, WorkerError::CreateUnixStream(_)),
            )
        });
        assert_listeners_closed("to create its socket pairs", status);
    }

    /// Same defect after the fork: the configuration cannot be sent to the new
    /// worker, so the launch fails with the child already running.
    #[test]
    fn a_launch_failing_to_send_the_configuration_closes_the_listeners() {
        let status = in_forked_child(|| {
            failed_launch_listener_status(
                received_listeners,
                config_that_cannot_be_sent(),
                || {},
                |error| matches!(error, WorkerError::SendConfig(_)),
            )
        });
        assert_listeners_closed("to send the configuration to the new worker", status);
    }

    /// A listener set outside the one-frame SCM contract must make the launch
    /// fail. The child has already forked and received its configuration, so
    /// the parent must also terminate and reap it before returning the error.
    #[test]
    fn a_launch_failing_to_forward_listeners_closes_them_and_reaps_the_child() {
        let status = in_forked_child(|| {
            failed_launch_listener_status(
                oversized_received_listeners,
                sendable_config(),
                || {},
                |error| matches!(error, WorkerError::SendListeners(_)),
            )
        });
        assert_listeners_closed("to forward its listener set", status);
    }

    /// `fork_main_into_worker` turns the main end of the worker's SCM socket
    /// into an `ScmSocket`, which has no `Drop`. Every error return after that
    /// point must close it: an automatic restart retries a failing launch at
    /// once, and each retry used to leave one more descriptor open in the main
    /// process until `EMFILE` stopped it from accepting a client or spawning
    /// a worker again.
    #[test]
    fn a_launch_failing_after_the_scm_socket_exists_leaves_no_descriptor_open() {
        let status = in_forked_child(|| {
            let state = ConfigState::new();
            let before = open_descriptors();
            let launched = fork_main_into_worker(
                "worker\0id",
                &config_that_cannot_be_sent(),
                "/nonexistent/sozu".to_string(),
                &state,
                None,
            );
            let after = open_descriptors();
            match launched {
                Ok(_) => LAUNCH_SUCCEEDED,
                Err(WorkerError::SendConfig(_)) => after.difference(&before).count() as libc::c_int,
                Err(_) => WRONG_EDGE,
            }
        });
        assert_eq!(
            status,
            Some(0),
            "a launch that failed after creating the SCM socket left that many descriptors \
             open in the main process ({LAUNCH_SUCCEEDED} = the launch did not fail, \
             {WRONG_EDGE} = it failed elsewhere, {SCENARIO_PANICKED} = panicked)"
        );
    }
}
