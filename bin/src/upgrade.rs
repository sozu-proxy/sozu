use std::{
    fs::File,
    io::{Seek, SeekFrom},
    os::unix::io::{AsRawFd, FromRawFd},
    os::unix::process::CommandExt,
    process::Command,
};

use anyhow::{bail, Context};
use futures_lite::future;
use libc::{self, pid_t};
use mio::net::UnixStream;
use nix::unistd::*;
use serde::{Deserialize, Serialize};

use tempfile::tempfile;

use sozu_command_lib::{
    channel::Channel, command::RunState, config::Config, proxy::ProxyRequest, state::ConfigState,
};

use crate::{
    command::{CommandServer, Worker},
    util,
};

#[derive(Deserialize, Serialize, Debug)]
pub struct SerializedWorker {
    pub fd: i32,
    pub pid: i32,
    pub id: u32,
    pub run_state: RunState,
    pub queue: Vec<ProxyRequest>,
    pub scm: i32,
}

impl SerializedWorker {
    pub fn from_worker(worker: &Worker) -> SerializedWorker {
        SerializedWorker {
            fd: worker.worker_channel_fd,
            pid: worker.pid,
            id: worker.id,
            run_state: worker.run_state,
            //token:      worker.token.clone().map(|Token(t)| t),
            queue: worker.queue.clone().into(),
            scm: worker.scm_socket.raw_fd(),
        }
    }
}

/// the data needed to start a new main process
#[derive(Deserialize, Serialize, Debug)]
pub struct UpgradeData {
    /// file descriptor of the unix command socket
    pub command_socket_fd: i32,
    //clients: ????
    pub config: Config,
    /// JSON serialized workers
    pub workers: Vec<SerializedWorker>,
    pub state: ConfigState,
    pub next_id: u32,
    //pub token_count: usize,
}

pub fn fork_main_into_new_main(
    executable_path: String,
    upgrade_data: UpgradeData,
) -> Result<(pid_t, Channel<(), bool>), anyhow::Error> {
    trace!("parent({})", unsafe { libc::getpid() });

    let mut upgrade_file =
        tempfile().with_context(|| "could not create temporary file for upgrade")?;

    util::disable_close_on_exec(upgrade_file.as_raw_fd())?;

    serde_json::to_writer(&mut upgrade_file, &upgrade_data)
        .with_context(|| "could not write upgrade data to temporary file")?;
    upgrade_file
        .seek(SeekFrom::Start(0))
        .with_context(|| "could not seek to beginning of file")?;

    let (old_to_new, new_to_old) = UnixStream::pair()?;

    util::disable_close_on_exec(new_to_old.as_raw_fd())?;

    let mut fork_confirmation_channel: Channel<(), bool> = Channel::new(
        old_to_new,
        upgrade_data.config.command_buffer_size,
        upgrade_data.config.max_command_buffer_size,
    );

    if let Err(e) = fork_confirmation_channel.blocking() {
        error!(
            "Could not block the fork confirmation channel: {}. This is not normal, you may need to restart sozu",
            e
        );
    }

    info!("launching new main");
    match unsafe { fork().with_context(|| "fork failed")? } {
        ForkResult::Parent { child } => {
            info!("main launched: {}", child);

            if let Err(e) = fork_confirmation_channel.nonblocking() {
                error!(
                    "Could not unblock the fork confirmation channel: {}. This is not normal, you may need to restart sozu", 
                    e
                );
            }

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

pub fn begin_new_main_process(
    new_to_old_channel_fd: i32,
    upgrade_file_fd: i32,
    command_buffer_size: usize,
    max_command_buffer_size: usize,
) -> anyhow::Result<()> {
    let mut fork_confirmation_channel: Channel<bool, ()> = Channel::new(
        unsafe { UnixStream::from_raw_fd(new_to_old_channel_fd) },
        command_buffer_size,
        max_command_buffer_size,
    );

    // DISCUSS: should we propagate the error instead of printing it?
    if let Err(e) = fork_confirmation_channel.blocking() {
        error!("Could not block the fork confirmation channel: {}", e);
    }

    let upgrade_file = unsafe { File::from_raw_fd(upgrade_file_fd) };
    let upgrade_data: UpgradeData =
        serde_json::from_reader(upgrade_file).with_context(|| "could not parse upgrade data")?;
    let config = upgrade_data.config.clone();

    util::setup_logging(&config);
    util::setup_metrics(&config).with_context(|| "Could not setup metrics")?;
    //info!("new main got upgrade data: {:?}", upgrade_data);

    let mut server = CommandServer::from_upgrade_data(upgrade_data)?;
    server.enable_cloexec_after_upgrade()?;
    info!("starting new main loop");
    match util::write_pid_file(&config) {
        Ok(()) => {
            fork_confirmation_channel
                .write_message(&true)
                .with_context(|| "Could not send confirmation of fork using the channel")?;
            future::block_on(async {
                server.run().await;
            });
            info!("main process stopped");
            Ok(())
        }
        Err(e) => {
            fork_confirmation_channel
                .write_message(&false)
                .with_context(|| "Could not send fork failure message using the channel")?;
            error!("Couldn't write PID file. Error: {:?}", e);
            error!("Couldn't upgrade main process");
            bail!("begin_new_main_process() failed");
        }
    }
}
