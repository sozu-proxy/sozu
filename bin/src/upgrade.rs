//! Master/worker hot-upgrade orchestration.
//!
//! Forks a child master, transfers listener and command-channel FDs over
//! the SCM_RIGHTS unix socket (`command/src/scm_socket.rs`), re-exec's the
//! `sozu` binary with the recovered state, and tears down the previous
//! master once the new one acks. Keeps the data plane uninterrupted by
//! handing off accepted listeners and existing worker FDs.

use std::{
    fs::File,
    io::{Error as IoError, Read, Seek, Write},
    os::unix::{
        io::{AsRawFd, FromRawFd},
        process::CommandExt,
    },
    process::Command,
};

use libc::pid_t;
use mio::net::UnixStream;
use nix::{
    errno::Errno,
    unistd::{ForkResult, fork},
};
use serde_json::Error as SerdeError;
use sozu_command_lib::{
    channel::{Channel, ChannelError},
    logging::{LogError, setup_logging_with_config},
    sd_notify,
};
use tempfile::tempfile;

use crate::{
    command::{
        server::{CommandHub, HubError, ServerError},
        upgrade::UpgradeData,
    },
    util::{self, UtilError},
};

#[derive(thiserror::Error, Debug)]
pub enum UpgradeError {
    #[error("could not create temporary state file for the upgrade: {0}")]
    CreateUpgradeFile(IoError),
    #[error("could not disable cloexec on {fd_name}'s file descriptor: {util_err}")]
    DisableCloexec {
        fd_name: String,
        util_err: UtilError,
    },
    #[error("could not create MIO pair of unix stream: {0}")]
    CreateUnixStream(IoError),
    #[error("could not rewind the temporary upgrade file: {0}")]
    Rewind(IoError),
    #[error("could not write upgrade data to temporary file: {0}")]
    SerdeWriteError(SerdeError),
    #[error("could not write upgrade data to temporary file: {0}")]
    WriteFile(IoError),
    #[error("could not read upgrade data from file: {0}")]
    ReadFile(IoError),
    #[error("could not read upgrade data to temporary file: {0}")]
    SerdeReadError(SerdeError),
    #[error("unix fork failed: {0}")]
    Fork(Errno),
    #[error("failed to set metrics on the new main process: {0}")]
    SetupMetrics(UtilError),
    #[error("could not write PID file of new main process: {0}")]
    WritePidFile(UtilError),
    #[error(
        "the channel failed to send confirmation of upgrade {result} to the old main process: {channel_err}"
    )]
    SendConfirmation {
        result: String,
        channel_err: ChannelError,
    },
    #[error(
        "Could not block the fork confirmation channel: {0}. This is not normal, you may need to restart sozu"
    )]
    BlockChannel(ChannelError),
    #[error("could not create a command hub from the upgrade data: {0}")]
    CreateHub(HubError),
    #[error("could not enable cloexec after upgrade: {0}")]
    EnableCloexec(ServerError),
    #[error("could not setup the logger: {0}")]
    SetupLogging(LogError),
}

/// unix-forks the main process
///
/// - Parent: meant to disappear after the child confirms it's alive
/// - Child: calls Sōzu's executable path with `sozu main [...]`
///
/// returns the pid of the new main, and a channel to get confirmation from the new main
pub fn fork_main_into_new_main(
    executable_path: String,
    upgrade_data: UpgradeData,
) -> Result<(pid_t, Channel<(), bool>), UpgradeError> {
    trace!("parent({})", std::process::id());

    let mut upgrade_file = tempfile().map_err(UpgradeError::CreateUpgradeFile)?;

    util::disable_close_on_exec(upgrade_file.as_raw_fd()).map_err(|util_err| {
        UpgradeError::DisableCloexec {
            fd_name: "upgrade-file".to_string(),
            util_err,
        }
    })?;

    // Snapshot the count of worker sessions being handed off BEFORE we move
    // `upgrade_data` into serialization, so we can reconcile it against what
    // the re-execed master will recover (round-trip below). Gated to debug:
    // the snapshots are read only inside the `#[cfg(debug_assertions)]` block.
    #[cfg(debug_assertions)]
    let workers_to_handoff = upgrade_data.workers.len();
    #[cfg(debug_assertions)]
    let boot_generation_handoff = upgrade_data.boot_generation;

    info!("Writing upgrade data to file");
    let upgrade_data_string =
        serde_json::to_string(&upgrade_data).map_err(UpgradeError::SerdeWriteError)?;

    // The serialized payload must round-trip: deserializing what we are about
    // to write back into `UpgradeData` and re-serializing must reproduce the
    // exact same bytes. `UpgradeData` field order is fixed by declaration, so
    // a stable serializer yields a canonical string — a mismatch would mean a
    // (de)serialization bug that would silently corrupt the recovered master
    // state on the other side of the re-exec. This is the cheap, in-process
    // half of the write->read round-trip that `begin_new_main_process`
    // completes after exec. (Guarded both let + assert: the re-parse only
    // exists for the check, so it is dead code stripped in release.)
    #[cfg(debug_assertions)]
    {
        match serde_json::from_str::<UpgradeData>(&upgrade_data_string) {
            Ok(reparsed) => {
                debug_assert_eq!(
                    reparsed.workers.len(),
                    workers_to_handoff,
                    "round-tripped upgrade data must preserve the worker-session count"
                );
                debug_assert_eq!(
                    reparsed.boot_generation, boot_generation_handoff,
                    "round-tripped upgrade data must preserve the boot generation"
                );
                let reserialized = serde_json::to_string(&reparsed)
                    .expect("re-serializing already-parsed upgrade data cannot fail");
                debug_assert_eq!(
                    reserialized, upgrade_data_string,
                    "upgrade data must serialize->deserialize->serialize identically (canonical round-trip)"
                );
            }
            Err(e) => debug_assert!(
                false,
                "upgrade data we just serialized must deserialize back: {e}"
            ),
        }
    }

    upgrade_file
        .write_all(upgrade_data_string.as_bytes())
        .map_err(UpgradeError::WriteFile)?;
    upgrade_file.rewind().map_err(UpgradeError::Rewind)?;

    let (old_to_new, new_to_old) = UnixStream::pair().map_err(UpgradeError::CreateUnixStream)?;

    // The descriptors the new binary inherits across the re-exec must be
    // distinct kernel objects — none handed off twice. Three are in play at
    // this site: the confirmation channel (`new_to_old`, passed as `--fd`),
    // the upgrade-state file (passed as `--upgrade-fd`), and the unix command
    // listener carried inside `UpgradeData` (`command_socket_fd`). A collision
    // would mean a single fd is being used for two roles after exec — an
    // fd-bookkeeping bug in the handoff.
    debug_assert!(
        new_to_old.as_raw_fd() != upgrade_file.as_raw_fd(),
        "confirmation-channel fd must not alias the upgrade-state file fd"
    );
    debug_assert!(
        new_to_old.as_raw_fd() != upgrade_data.command_socket_fd,
        "confirmation-channel fd must not alias the inherited command-socket fd"
    );
    debug_assert!(
        upgrade_file.as_raw_fd() != upgrade_data.command_socket_fd,
        "upgrade-state file fd must not alias the inherited command-socket fd"
    );

    util::disable_close_on_exec(new_to_old.as_raw_fd()).map_err(|util_err| {
        UpgradeError::DisableCloexec {
            fd_name: "new-main-to-old-main-channel".to_string(),
            util_err,
        }
    })?;

    let mut fork_confirmation_channel: Channel<(), bool> = Channel::new(
        old_to_new,
        upgrade_data.config.command_buffer_size,
        upgrade_data.config.max_command_buffer_size,
    );

    fork_confirmation_channel
        .blocking()
        .map_err(UpgradeError::BlockChannel)?;

    info!("launching new main");
    // SAFETY: `fork` is unsafe because the child must avoid touching
    // shared mutable state inherited from the parent. The child branch
    // below restricts itself to `exec` (via `Command::exec`), which
    // replaces the process image entirely — no inherited state matters
    // after that point.
    match unsafe { fork().map_err(UpgradeError::Fork)? } {
        ForkResult::Parent { child } => {
            // The parent branch of a successful `fork()` always observes the
            // child's pid, which is strictly positive (0 is the child branch;
            // a failure returns `Err` above). A non-positive value here would
            // mean we mis-classified the fork outcome and would report a bogus
            // new-main pid to the operator.
            debug_assert!(
                child.as_raw() > 0,
                "fork parent branch must observe a strictly positive new-main pid"
            );
            info!("new main launched, with pid {}", child);

            Ok((child.into(), fork_confirmation_channel))
        }
        ForkResult::Child => {
            trace!("child({}):\twill spawn a child", std::process::id());

            // #515 scope clarification: this is the operator-triggered
            // master hot-upgrade path. The whole point is to re-exec the
            // **new** on-disk binary the operator just installed at
            // `executable_path` — a path-based `Command::new(...)` is the
            // intended behaviour. The race-free fd-based exec path
            // (`get_executable_exec_path()`, used in `bin/src/worker.rs`
            // for worker auto-restart) deliberately does NOT apply here:
            // a worker that respawns mid-package-install must match the
            // running master's version (race-free), but the master that
            // hot-upgrades is by design switching to a different version.
            //
            // Operator-driven schema mismatch on `upgrade-main` (the new
            // binary's proto schema being incompatible with the running
            // master's `UpgradeData` payload written via
            // `serde_json::to_string` above) is a separate concern: the
            // newly-execed master will fail to parse the inherited state
            // and the upgrade aborts cleanly. Operators must verify
            // proto compatibility before swapping the on-disk binary.
            let res = Command::new(executable_path)
                .arg("main")
                .arg("--fd")
                .arg(new_to_old.as_raw_fd().to_string())
                .arg("--upgrade-fd")
                .arg(upgrade_file.as_raw_fd().to_string())
                .arg("--command-buffer-size")
                .arg(upgrade_data.config.command_buffer_size.to_string())
                .arg("--max-command-buffer-size")
                .arg(upgrade_data.config.max_command_buffer_size.to_string())
                .exec();

            error!("exec call failed: {:?}", res);
            unreachable!();
        }
    }
}

/// Called by the child of a main process fork.
/// Starts new main process with upgrade data, notifies the old main process.
/// Only called from the binary entry point (main.rs), not from the library.
#[allow(dead_code)]
pub fn begin_new_main_process(
    new_to_old_channel_fd: i32,
    upgrade_file_fd: i32,
    command_buffer_size: u64,
    max_command_buffer_size: u64,
) -> Result<(), UpgradeError> {
    // Both descriptors were handed to us across the re-exec by the old master
    // (`fork_main_into_new_main`), each derived from a live `as_raw_fd()`: they
    // are valid (>= 0) and reference two distinct kernel objects (the
    // confirmation channel vs. the upgrade-state file). Aliasing them would be
    // an fd-bookkeeping bug on the handoff side. (Stays a `debug_assert!`: a
    // genuinely bad descriptor surfaces as a returned channel / read error.)
    debug_assert!(
        new_to_old_channel_fd >= 0,
        "inherited new-main-to-old-main channel fd must be a valid descriptor"
    );
    debug_assert!(
        upgrade_file_fd >= 0,
        "inherited upgrade-state file fd must be a valid descriptor"
    );
    debug_assert!(
        new_to_old_channel_fd != upgrade_file_fd,
        "the confirmation channel and upgrade-state file must be two distinct fds"
    );

    let mut fork_confirmation_channel: Channel<bool, ()> = Channel::new(
        // SAFETY: `new_to_old_channel_fd` was just inherited from the
        // pre-exec parent process via the `--fd` CLI argument. It is a valid
        // open descriptor with no other owner inside this freshly-execed
        // process. Ownership transfers to the `UnixStream`, whose `Drop`
        // closes the descriptor.
        unsafe { UnixStream::from_raw_fd(new_to_old_channel_fd) },
        command_buffer_size,
        max_command_buffer_size,
    );

    // DISCUSS: should we propagate the error instead of printing it?
    if let Err(e) = fork_confirmation_channel.blocking() {
        error!("Could not block the fork confirmation channel: {}", e);
    }

    println!("reading upgrade data from file");

    // SAFETY: `upgrade_file_fd` was just inherited from the pre-exec parent
    // process via the `--upgrade-fd` CLI argument. It is a valid open
    // descriptor with no other owner inside this freshly-execed process.
    // Ownership transfers to the `File`, whose `Drop` closes the descriptor.
    let mut upgrade_file = unsafe { File::from_raw_fd(upgrade_file_fd) };
    let mut content = String::new();
    let _ = upgrade_file
        .read_to_string(&mut content)
        .map_err(UpgradeError::ReadFile)?;

    // This is the read side of the write->read round-trip started in
    // `fork_main_into_new_main`: the old master wrote a non-empty serialized
    // `UpgradeData` object. The recovered payload must therefore be non-empty
    // and start with the JSON object delimiter. An empty/truncated read means
    // the fd handoff or the file rewind was botched — a malformed payload
    // still surfaces as a returned `SerdeReadError` below, this only flags the
    // logic bug earlier and loudly.
    debug_assert!(
        !content.is_empty(),
        "recovered upgrade state must not be empty (write->read round-trip)"
    );
    debug_assert!(
        content.trim_start().starts_with('{'),
        "recovered upgrade state must be a serialized JSON object"
    );

    let upgrade_data: UpgradeData =
        serde_json::from_str(&content).map_err(UpgradeError::SerdeReadError)?;

    let config = upgrade_data.config.clone();

    println!("Setting up logging");

    setup_logging_with_config(&config, "MAIN").map_err(UpgradeError::SetupLogging)?;
    util::setup_metrics(&config).map_err(UpgradeError::SetupMetrics)?;

    let mut command_hub =
        CommandHub::from_upgrade_data(upgrade_data).map_err(UpgradeError::CreateHub)?;

    command_hub
        .enable_cloexec_after_upgrade()
        .map_err(UpgradeError::EnableCloexec)?;

    util::write_pid_file(&config).map_err(UpgradeError::WritePidFile)?;

    fork_confirmation_channel
        .write_message(&true)
        .map_err(|channel_err| UpgradeError::SendConfirmation {
            result: "success".to_string(),
            channel_err,
        })?;

    info!("starting new main loop");

    // #228: tell systemd that the new master pid takes over from the
    // pre-exec one (`Type=notify` + `NotifyAccess=main` are required
    // in the unit file for this to be honoured), then signal READY=1
    // for the post-exec master. The old master sent `RELOADING=1`
    // before forking, so systemd is in `reloading` state and will
    // honour MAINPID= even before READY=1 swaps the unit back to
    // active. No-op when `$NOTIFY_SOCKET` is unset.
    let new_pid = std::process::id();
    if let Err(e) = sd_notify::main_pid(new_pid) {
        warn!("could not notify systemd MAINPID={}: {}", new_pid, e);
    }
    match sd_notify::notify(sd_notify::STATE_READY) {
        Ok(true) => debug!(
            "notified systemd post-upgrade: MAINPID={}, READY=1",
            new_pid
        ),
        Ok(false) => {}
        Err(e) => warn!("could not notify systemd READY=1: {}", e),
    }

    command_hub.run();

    // The new master's `command_hub.run()` exits on graceful shutdown
    // (the upgrade-handoff path is exclusive to the OLD master that
    // forked us); STOPPING=1 here is unambiguous.
    if let Err(e) = sd_notify::notify(sd_notify::STATE_STOPPING) {
        warn!("could not notify systemd STOPPING=1: {}", e);
    }

    info!("main process stopped");
    Ok(())
}
