use anyhow::Context;
use libc;
use nix::fcntl::{fcntl, FcntlArg, FdFlag};
use std::fs::File;
use std::io::Write;
use std::os::unix::io::RawFd;

use crate::logging;
use anyhow;
use sozu::metrics;
use sozu_command::config::Config;

pub fn enable_close_on_exec(fd: RawFd) -> Result<i32, anyhow::Error> {
    let file_descriptor =
        fcntl(fd, FcntlArg::F_GETFD).with_context(|| "could not get file descriptor flags")?;

    let mut new_flags = FdFlag::from_bits(file_descriptor)
        .ok_or_else(|| anyhow::format_err!("could not convert flags for file descriptor"))?;

    new_flags.insert(FdFlag::FD_CLOEXEC);

    fcntl(fd, FcntlArg::F_SETFD(new_flags)).with_context(|| "could not set file descriptor flags")
}

// FD_CLOEXEC is set by default on every fd in Rust standard lib,
// so we need to remove the flag on the client, otherwise
// it won't be accessible
pub fn disable_close_on_exec(fd: RawFd) -> Result<i32, anyhow::Error> {
    let old_flags =
        fcntl(fd, FcntlArg::F_GETFD).with_context(|| "could not get file descriptor flags")?;

    let mut new_flags = FdFlag::from_bits(old_flags)
        .ok_or_else(|| anyhow::format_err!("could not convert flags for file descriptor"))?;

    new_flags.remove(FdFlag::FD_CLOEXEC);

    fcntl(fd, FcntlArg::F_SETFD(new_flags)).with_context(|| "could not set file descriptor flags")
}

pub fn setup_logging(config: &Config) {
    //FIXME: should have an id for the main too
    logging::setup(
        "MAIN".to_string(),
        &config.log_level,
        &config.log_target,
        config.log_access_target.as_ref().map(|s| s.as_str()),
    );
}

pub fn setup_metrics(config: &Config) {
    if let Some(ref metrics) = config.metrics.as_ref() {
        metrics::setup(
            &metrics.address,
            "MAIN",
            metrics.tagged_metrics,
            metrics.prefix.clone(),
        );
    }
}

pub fn write_pid_file(config: &Config) -> Result<(), anyhow::Error> {
    let pid_file_path = match config.pid_file_path {
        Some(ref pid_file_path) => Some(pid_file_path.as_ref()),
        None => option_env!("SOZU_PID_FILE_PATH"),
    };

    if let Some(ref pid_file_path) = pid_file_path {
        let mut file = File::create(pid_file_path)?;

        let pid = unsafe { libc::getpid() };

        file.write_all(format!("{}", pid).as_bytes())?;
        file.sync_all()?;
    }
    Ok(())
}
