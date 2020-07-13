use nix::fcntl::{fcntl,FcntlArg,FdFlag};
use std::os::unix::io::RawFd;
use std::fs::File;
use std::io::Write;
use libc;

use crate::logging;
use sozu_command::config::Config;
use sozu::metrics;

pub fn enable_close_on_exec(fd: RawFd) -> Option<i32> {
  fcntl(fd, FcntlArg::F_GETFD).map_err(|e| {
    error!("could not get file descriptor flags: {:?}", e);
  }).ok().and_then(FdFlag::from_bits).and_then(|mut new_flags| {
    new_flags.insert(FdFlag::FD_CLOEXEC);
    fcntl(fd, FcntlArg::F_SETFD(new_flags)).map_err(|e| {
      error!("could not set file descriptor flags: {:?}", e);
    }).ok()
  })
}

// FD_CLOEXEC is set by default on every fd in Rust standard lib,
// so we need to remove the flag on the client, otherwise
// it won't be accessible
pub fn disable_close_on_exec(fd: RawFd) -> Option<i32> {
  fcntl(fd, FcntlArg::F_GETFD).map_err(|e| {
    error!("could not get file descriptor flags: {:?}", e);
  }).ok().and_then(FdFlag::from_bits).and_then(|mut new_flags| {
    new_flags.remove(FdFlag::FD_CLOEXEC);
    fcntl(fd, FcntlArg::F_SETFD(new_flags)).map_err(|e| {
      error!("could not set file descriptor flags: {:?}", e);
    }).ok()
  })
}

pub fn setup_logging(config: &Config) {
  //FIXME: should have an id for the main too
  logging::setup("MAIN".to_string(), &config.log_level,
    &config.log_target, config.log_access_target.as_ref().map(|s| s.as_str()));
}

pub fn setup_metrics(config: &Config) {
  if let Some(ref metrics) = config.metrics.as_ref() {
    metrics::setup(&metrics.address, "MAIN", metrics.tagged_metrics, metrics.prefix.clone());
  }
}

pub fn write_pid_file(config: &Config) -> Result<(), String> {
  let pid_file_path = match config.pid_file_path {
    Some(ref pid_file_path) => Some(pid_file_path.as_ref()),
    None => option_env!("SOZU_PID_FILE_PATH")
  };

  if let Some(ref pid_file_path) = pid_file_path {
    File::create(pid_file_path)
      .and_then(|mut file| {
        let pid = unsafe { libc::getpid() };
        file
          .write_all(format!("{}", pid).as_bytes())
          .map(|()| file)
      })
      .and_then(|file| file.sync_all())
      .map(|()| ())
      .or_else(|err| Err(format!("Couldn't write the PID file to {}. Error: {:?}", pid_file_path, err)))
  } else {
    Ok(())
  }
}
