use mio_uds::UnixStream;
use mio::Token;
use libc::{self,pid_t};
use std::collections::{HashMap,HashSet};
use std::process::Command;
use std::os::unix::process::CommandExt;
use std::os::unix::io::{AsRawFd,FromRawFd};
use nix::unistd::*;
use nix::fcntl::{fcntl,FcntlArg,FdFlag,FD_CLOEXEC};

use sozu::channel::Channel;
use sozu::messages::{Order,OrderMessage};
use sozu_command::config::Config;
use sozu_command::state::ConfigState;
use sozu_command::data::RunState;

use logging;
use command::{CommandServer,Worker};
use worker::get_executable_path;

#[derive(Deserialize,Serialize,Debug)]
pub struct SerializedWorker {
  pub fd:         i32,
  pub pid:        i32,
  pub id:         u32,
  pub run_state:  RunState,
  pub token:      Option<usize>,
  pub queue:      Vec<OrderMessage>,
}

impl SerializedWorker {
  pub fn from_proxy(proxy: &Worker) -> SerializedWorker {
    SerializedWorker {
      fd:         proxy.channel.sock.as_raw_fd(),
      pid:        proxy.pid,
      id:         proxy.id,
      run_state:  proxy.run_state.clone(),
      token:      proxy.token.clone().map(|Token(t)| t),
      queue:      proxy.queue.clone().into(),
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
  pub token_count: usize,
  //pub order_state: HashMap<String, HashSet<usize>>,
}

pub fn start_new_master_process(upgrade_data: UpgradeData) -> (pid_t, Channel<UpgradeData,bool>) {
  trace!("parent({})", unsafe { libc::getpid() });

  let (server, client) = UnixStream::pair().unwrap();

  // FD_CLOEXEC is set by default on every fd in Rust standard lib,
  // so we need to remove the flag on the client, otherwise
  // it won't be accessible
  let cl_flags = fcntl(client.as_raw_fd(), FcntlArg::F_GETFD).unwrap();
  let mut new_cl_flags = FdFlag::from_bits(cl_flags).unwrap();
  new_cl_flags.remove(FD_CLOEXEC);
  fcntl(client.as_raw_fd(), FcntlArg::F_SETFD(new_cl_flags));

  let channel_buffer_size = upgrade_data.config.command_buffer_size.unwrap_or(10000);
  let channel_max_buffer_size = channel_buffer_size * 2;

  let mut command: Channel<UpgradeData,bool> = Channel::new(
    server,
    channel_buffer_size,
    channel_max_buffer_size
  );
  command.set_nonblocking(false);

  let path = unsafe { get_executable_path() };

  info!("launching worker");
  //FIXME: remove the expect, return a result?
  match fork().expect("fork failed") {
    ForkResult::Parent{ child } => {
      info!("worker launched: {}", child);
      command.write_message(&upgrade_data);
      command.set_nonblocking(true);

      return (child, command);
    }
    ForkResult::Child => {
      trace!("child({}):\twill spawn a child", unsafe { libc::getpid() });
      Command::new(path.to_str().unwrap())
        .arg("upgrade")
        .arg("--fd")
        .arg(client.as_raw_fd().to_string())
        .arg("--channel-buffer-size")
        .arg(channel_buffer_size.to_string())
        .exec();

      unreachable!();
    }
  }
}

pub fn begin_new_master_process(fd: i32, channel_buffer_size: usize) {
  let mut command: Channel<bool,UpgradeData> = Channel::new(
    unsafe { UnixStream::from_raw_fd(fd) },
    channel_buffer_size,
    channel_buffer_size *2
  );

  command.set_blocking(true);

  let upgrade_data = command.read_message().expect("new master could not read upgradz_data from socket");
  //FIXME: should have an id for the master too
  logging::setup("MASTER".to_string(), &upgrade_data.config.log_level, &upgrade_data.config.log_target);
  info!("new master got upgrade data: {:?}", upgrade_data);

  let mut server = CommandServer::from_upgrade_data(upgrade_data);
  server.enable_cloexec_after_upgrade();
  info!("starting new master loop");
  command.write_message(&true);
  server.run();
  info!("master process stopped");

}
