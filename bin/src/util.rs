use std::{fs::File, io::Write, os::unix::io::RawFd};

use anyhow::Context;

use log::LevelFilter;
use nix::fcntl::{fcntl, FcntlArg, FdFlag};
use tracing::{subscriber, Level};

use sozu::metrics;
use sozu_command_lib::config::Config;
use tracing_subscriber::{fmt, reload};

// use crate::logging;

pub fn enable_close_on_exec(fd: RawFd) -> Result<i32, anyhow::Error> {
    let file_descriptor =
        fcntl(fd, FcntlArg::F_GETFD).with_context(|| "could not get file descriptor flags")?;

    let mut new_flags = FdFlag::from_bits(file_descriptor)
        .ok_or_else(|| anyhow::format_err!("could not convert flags for file descriptor"))?;

    new_flags.insert(FdFlag::FD_CLOEXEC);

    fcntl(fd, FcntlArg::F_SETFD(new_flags)).with_context(|| "could not set file descriptor flags")
}

/// FD_CLOEXEC is set by default on every fd in Rust standard lib,
/// so we need to remove the flag on the client, otherwise
/// it won't be accessible
pub fn disable_close_on_exec(fd: RawFd) -> Result<i32, anyhow::Error> {
    let old_flags =
        fcntl(fd, FcntlArg::F_GETFD).with_context(|| "could not get file descriptor flags")?;

    let mut new_flags = FdFlag::from_bits(old_flags)
        .ok_or_else(|| anyhow::format_err!("could not convert flags for file descriptor"))?;

    new_flags.remove(FdFlag::FD_CLOEXEC);

    fcntl(fd, FcntlArg::F_SETFD(new_flags)).with_context(|| "could not set file descriptor flags")
}

/*
pub fn setup_logging(config: &Config, tag: &str) {
    //FIXME: should have an id for the main too
    logging::setup(
        tag.to_owned(),
        &config.log_level,
        &config.log_target,
        config.log_access_target.as_deref(),
    );
}
*/


pub fn setup_metrics(config: &Config) -> anyhow::Result<()> {
    if let Some(metrics) = config.metrics.as_ref() {
        return metrics::setup(
            &metrics.address,
            "MAIN",
            metrics.tagged_metrics,
            metrics.prefix.clone(),
        );
    }
    Ok(())
}

pub fn write_pid_file(config: &Config) -> Result<(), anyhow::Error> {
    let pid_file_path: Option<&str> = config
        .pid_file_path
        .as_ref()
        .map(|pid_file_path| pid_file_path.as_ref());

    if let Some(pid_file_path) = pid_file_path {
        let mut file = File::create(pid_file_path)?;

        let pid = unsafe { libc::getpid() };

        file.write_all(format!("{}", pid).as_bytes())?;
        file.sync_all()?;
    }
    Ok(())
}
