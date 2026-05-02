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

    info!("Writing upgrade data to file");
    let upgrade_data_string =
        serde_json::to_string(&upgrade_data).map_err(UpgradeError::SerdeWriteError)?;
    upgrade_file
        .write_all(upgrade_data_string.as_bytes())
        .map_err(UpgradeError::WriteFile)?;
    upgrade_file.rewind().map_err(UpgradeError::Rewind)?;

    let (old_to_new, new_to_old) = UnixStream::pair().map_err(UpgradeError::CreateUnixStream)?;

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
