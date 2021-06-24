use mio::net::UnixStream;
use libc::{self,pid_t};
use std::process::Command;
use std::os::unix::process::CommandExt;
use std::os::unix::io::{AsRawFd,FromRawFd};
use std::fs::File;
use std::io::{Seek,SeekFrom};
use nix::unistd::*;
use serde_json;
use tempfile::tempfile;
use blocking::block_on;

use sozu_command::config::Config;
use sozu_command::command::RunState;
use sozu_command::channel::Channel;
use sozu_command::state::ConfigState;
use sozu_command::proxy::ProxyRequest;

use crate::util;
use crate::command::{CommandServer,Worker};

#[derive(Deserialize,Serialize,Debug)]
pub struct SerializedWorker {
  pub fd:         i32,
  pub pid:        i32,
  pub id:         u32,
  pub run_state:  RunState,
  pub queue:      Vec<ProxyRequest>,
  pub scm:        i32,
}

impl SerializedWorker {
  pub fn from_worker(worker: &Worker) -> SerializedWorker {
    SerializedWorker {
      fd:         worker.fd,
      pid:        worker.pid,
      id:         worker.id,
      run_state:  worker.run_state.clone(),
      //token:      worker.token.clone().map(|Token(t)| t),
      queue:      worker.queue.clone().into(),
      scm:        worker.scm.raw_fd(),
    }
  }
}

#[derive(Deserialize,Serialize,Debug)]
pub struct UpgradeData {
  pub command:     i32,
  //clients: ????
  pub config:      Config,
  pub workers:     Vec<SerializedWorker>,
  pub state:       ConfigState,
  pub next_id:     u32,
  //pub token_count: usize,
}

pub fn start_new_main_process(executable_path: String, upgrade_data: UpgradeData) -> (pid_t, Channel<(),bool>) {
  trace!("parent({})", unsafe { libc::getpid() });

  let mut upgrade_file = tempfile().expect("could not create temporary file for upgrade");

  util::disable_close_on_exec(upgrade_file.as_raw_fd());

  serde_json::to_writer(&mut upgrade_file, &upgrade_data).expect("could not write upgrade data to temporary file");
  upgrade_file.seek(SeekFrom::Start(0)).expect("could not seek to beginning of file");

  let (server, client) = UnixStream::pair().unwrap();

  util::disable_close_on_exec(client.as_raw_fd());

  let mut command: Channel<(),bool> = Channel::new(
    server,
    upgrade_data.config.command_buffer_size,
    upgrade_data.config.max_command_buffer_size
  );
  command.set_nonblocking(false);

  info!("launching new main");
  //FIXME: remove the expect, return a result?
  match fork().expect("fork failed") {
    ForkResult::Parent{ child } => {
      info!("main launched: {}", child);
      command.set_nonblocking(true);

      return (child.into(), command);
    }
    ForkResult::Child => {
      trace!("child({}):\twill spawn a child", unsafe { libc::getpid() });
      let res = Command::new(executable_path)
        .arg("upgrade")
        .arg("--fd")
        .arg(client.as_raw_fd().to_string())
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

pub fn begin_new_main_process(fd: i32, upgrade_fd: i32, command_buffer_size: usize, max_command_buffer_size: usize) {
  let mut command: Channel<bool,()> = Channel::new(
    unsafe { UnixStream::from_raw_fd(fd) },
    command_buffer_size,
    max_command_buffer_size
  );

  command.set_blocking(true);

  let upgrade_file = unsafe { File::from_raw_fd(upgrade_fd) };
  let upgrade_data: UpgradeData = serde_json::from_reader(upgrade_file).expect("could not parse upgrade data");
  let config = upgrade_data.config.clone();

  util::setup_logging(&config);
  util::setup_metrics(&config);
  //info!("new main got upgrade data: {:?}", upgrade_data);

  let mut server = CommandServer::from_upgrade_data(upgrade_data);
  server.enable_cloexec_after_upgrade();
  info!("starting new main loop");
  match util::write_pid_file(&config) {
    Ok(()) => {
      command.write_message(&true);
      block_on(async {
        server.run().await;
      });
      info!("main process stopped");
    },
    Err(e) => {
      command.write_message(&false);
      error!("Couldn't write PID file. Error: {:?}", e);
      error!("Couldn't upgrade main process");
    }
  }
}
