use mio_uds::UnixStream;
use mio::Ready;
use libc::{self,c_char,uint32_t,int32_t,pid_t};
use std::io::{Seek,SeekFrom};
use std::fs::File;
use std::ffi::CString;
use std::iter::repeat;
use std::ptr::null_mut;
use std::process::Command;
use std::net::ToSocketAddrs;
use std::os::unix::process::CommandExt;
use std::os::unix::io::{AsRawFd,FromRawFd,RawFd};
use tempfile::tempfile;
use serde_json;
use nix;
use nix::unistd::*;

use sozu_command::config::Config;
use sozu_command::channel::Channel;
use sozu_command::state::ConfigState;
use sozu_command::scm_socket::ScmSocket;
use sozu_command::messages::{OrderMessage,OrderMessageAnswer};
use sozu::network::proxy::Server;

use util;
use logging;
use command::Worker;

pub fn start_workers(config: &Config) -> nix::Result<Vec<Worker>> {
  let state = ConfigState::new();
  let mut workers = Vec::new();
  for index in 0..config.worker_count {
    match start_worker_process(&index.to_string(), config, &state) {
      Ok((pid, command, scm)) => {
        let w =  Worker::new(index as u32, pid, command, ScmSocket::new(scm), config);
        workers.push(w);
      },
      Err(e) => return Err(e)
    };
  }
  Ok(workers)
}

pub fn start_worker(id: u32, config: &Config, state: &ConfigState) -> nix::Result<Worker> {
  match start_worker_process(&id.to_string(), config, state) {
    Ok((pid, command, scm)) => {
      let w = Worker::new(id, pid, command,  ScmSocket::new(scm), config);
      Ok(w)
    },
    Err(e) => Err(e)
  }
}

pub fn begin_worker_process(fd: i32, scm: i32, configuration_state_fd: i32, id: i32, channel_buffer_size: usize) {
  let mut command: Channel<OrderMessageAnswer,Config> = Channel::new(
    unsafe { UnixStream::from_raw_fd(fd) },
    channel_buffer_size,
    channel_buffer_size * 2
  );

  command.set_nonblocking(false);

  let configuration_state_file = unsafe { File::from_raw_fd(configuration_state_fd) };
  let config_state: ConfigState = serde_json::from_reader(configuration_state_file)
    .expect("could not parse configuration state data");

  let proxy_config = command.read_message().expect("worker could not read configuration from socket");
  //println!("got message: {:?}", proxy_config);

  logging::setup(format!("{}-{:02}", "WRK", id), &proxy_config.log_level, &proxy_config.log_target);
  info!("worker {} starting...", id);

  command.set_nonblocking(true);
  let mut command: Channel<OrderMessageAnswer,OrderMessage> = command.into();
  command.readiness.insert(Ready::readable());

  if let Some(ref metrics) = proxy_config.metrics.as_ref() {
    metrics_set_up!(&metrics.address[..], metrics.port);
    gauge!("sozu.worker.TEST", 42);
  }

  let mut server = Server::new_from_config(command, ScmSocket::new(scm), proxy_config, config_state);

  info!("{} starting event loop", id);
  server.run();
  info!("{} ending event loop", id);
}

pub fn start_worker_process(id: &str, config: &Config, state: &ConfigState) -> nix::Result<(pid_t, Channel<OrderMessage,OrderMessageAnswer>, RawFd)> {
  trace!("parent({})", unsafe { libc::getpid() });

  let mut state_file = tempfile().expect("could not create temporary file for configuration state");
  util::disable_close_on_exec(state_file.as_raw_fd());

  serde_json::to_writer(&mut state_file, state).expect("could not write upgrade data to temporary file");
  state_file.seek(SeekFrom::Start(0)).expect("could not seek to beginning of file");

  let (server, client) = UnixStream::pair().unwrap();
  let (scm_server, scm_client) = UnixStream::pair().unwrap();

  util::disable_close_on_exec(client.as_raw_fd());

  let channel_buffer_size = config.channel_buffer_size;
  //FIXME
  let channel_max_buffer_size = config.channel_buffer_size * 2;

  let mut command: Channel<Config,OrderMessageAnswer> = Channel::new(
    server,
    channel_buffer_size,
    channel_max_buffer_size
  );
  command.set_nonblocking(false);

  let path = unsafe { get_executable_path() };

  info!("{} launching worker", id);
  debug!("executable path is {}", path);
  match fork() {
    Ok(ForkResult::Parent{ child }) => {
      info!("{} worker launched: {}", id, child);
      command.write_message(config);
      command.set_nonblocking(true);

      let command: Channel<OrderMessage,OrderMessageAnswer> = command.into();
      Ok((child, command, scm_server.as_raw_fd()))
    },
    Ok(ForkResult::Child) => {
      trace!("child({}):\twill spawn a child", unsafe { libc::getpid() });
      Command::new(path)
        .arg("worker")
        .arg("--id")
        .arg(id)
        .arg("--fd")
        .arg(client.as_raw_fd().to_string())
        .arg("--scm")
        .arg(scm_client.as_raw_fd().to_string())
        .arg("--configuration-state-fd")
        .arg(state_file.as_raw_fd().to_string())
        .arg("--channel-buffer-size")
        .arg(channel_buffer_size.to_string())
        .exec();

      unreachable!();
    },
    Err(e) => {
      error!("Error during fork(): {}", e);
      Err(e)
    }
  }
}

#[cfg(target_os = "linux")]
pub unsafe fn get_executable_path() -> String {
  use std::fs;

  let path         = fs::read_link("/proc/self/exe").expect("/proc/self/exe doesn't exist");
  let mut path_str = path.into_os_string().into_string().expect("Failed to convert PathBuf to String");

  if path_str.ends_with(" (deleted)") {
    // The kernel appends " (deleted)" to the symlink when the original executable has been replaced
    let len = path_str.len();
    path_str.truncate(len - 10)
  }

  path_str
}

#[cfg(target_os = "macos")]
extern {
  pub fn _NSGetExecutablePath(buf: *mut c_char, size: *mut uint32_t) -> int32_t;
}

#[cfg(target_os = "macos")]
pub unsafe fn get_executable_path() -> String {
  let capacity = 2000;
  let mut temp:Vec<u8> = Vec::with_capacity(capacity);
  temp.extend(repeat(0).take(capacity));
  let pathbuf = CString::from_vec_unchecked(temp);
  let ptr = pathbuf.into_raw();

  let mut size:uint32_t = capacity as u32;
  if _NSGetExecutablePath(ptr, &mut size) == 0 {

    let mut temp2:Vec<u8> = Vec::with_capacity(capacity);
    temp2.extend(repeat(0).take(capacity));
    let pathbuf2 = CString::from_vec_unchecked(temp2);
    let ptr2 = pathbuf2.into_raw();

    if libc::realpath(ptr, ptr2) != null_mut() {
      let path = CString::from_raw(ptr2);
      path.to_str().expect("failed to convert CString to String").to_string()
    } else {
      panic!();
    }
  } else {
    panic!("buffer too small");
  }
}
