use nix::fcntl::{fcntl,FcntlArg,FdFlag};
use std::os::unix::io::RawFd;

use logging;
use sozu_command::config::Config;

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
  //FIXME: should have an id for the master too
  logging::setup("MASTER".to_string(), &config.log_level,
    &config.log_target, config.log_access_target.as_ref().map(|s| s.as_str()));
}

pub fn setup_metrics(config: &Config) {
  if let Some(ref metrics) = config.metrics.as_ref() {
    metrics_set_up!(&metrics.address[..], metrics.port, "MASTER".to_string(), metrics.tagged_metrics);
  }
}
