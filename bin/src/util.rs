use nix::fcntl::{fcntl,FcntlArg,FdFlag,FD_CLOEXEC};
use std::os::unix::io::RawFd;

pub fn enable_close_on_exec(fd: RawFd) -> Option<i32> {
  fcntl(fd, FcntlArg::F_GETFD).map_err(|e| {
    error!("could not get file descriptor flags: {:?}", e);
  }).ok().and_then(FdFlag::from_bits).and_then(|mut new_flags| {
    new_flags.insert(FD_CLOEXEC);
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
    new_flags.remove(FD_CLOEXEC);
    fcntl(fd, FcntlArg::F_SETFD(new_flags)).map_err(|e| {
      error!("could not set file descriptor flags: {:?}", e);
    }).ok()
  })
}
