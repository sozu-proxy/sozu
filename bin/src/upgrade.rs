use std::{
    fs::File,
    io::{Error as IoError, Write},
    io::{Read, Seek},
    os::unix::io::{AsRawFd, FromRawFd},
    os::unix::process::CommandExt,
    process::Command,
};

use libc::pid_t;
use mio::net::UnixStream;
use nix::{
    errno::Errno,
    unistd::{fork, ForkResult},
};
use serde_json::Error as SerdeError;
use tempfile::tempfile;

use sozu_command_lib::{
    channel::{Channel, ChannelError},
    logging::setup_logging_with_config,
};

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
    #[error("Could not block the fork confirmation channel: {0}. This is not normal, you may need to restart sozu")]
    BlockChannel(ChannelError),
    #[error("could not create a command hub from the upgrade data: {0}")]
    CreateHub(HubError),
    #[error("could not enable cloexec after upgrade: {0}")]
    EnableCloexec(ServerError),
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
    trace!("parent({})", unsafe { libc::getpid() });

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
    match unsafe { fork().map_err(UpgradeError::Fork)? } {
        ForkResult::Parent { child } => {
            info!("new main launched, with pid {}", child);

            Ok((child.into(), fork_confirmation_channel))
        }
        ForkResult::Child => {
            trace!("child({}):\twill spawn a child", unsafe { libc::getpid() });
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
/// Starts new main process with upgrade data, notifies the old main process
pub fn begin_new_main_process(
    new_to_old_channel_fd: i32,
    upgrade_file_fd: i32,
    command_buffer_size: u64,
    max_command_buffer_size: u64,
) -> Result<(), UpgradeError> {
    let mut fork_confirmation_channel: Channel<bool, ()> = Channel::new(
        unsafe { UnixStream::from_raw_fd(new_to_old_channel_fd) },
        command_buffer_size,
        max_command_buffer_size,
    );

    // DISCUSS: should we propagate the error instead of printing it?
    if let Err(e) = fork_confirmation_channel.blocking() {
        error!("Could not block the fork confirmation channel: {}", e);
    }

    println!("reading upgrade data from file");

    let mut upgrade_file = unsafe { File::from_raw_fd(upgrade_file_fd) };
    let mut content = String::new();
    let _ = upgrade_file
        .read_to_string(&mut content)
        .map_err(UpgradeError::ReadFile)?;

    let upgrade_data: UpgradeData =
        serde_json::from_str(&content).map_err(UpgradeError::SerdeReadError)?;

    let config = upgrade_data.config.clone();

    println!("Setting up logging");

    setup_logging_with_config(&config, "MAIN");
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
    command_hub.run();

    info!("main process stopped");
    Ok(())
}
