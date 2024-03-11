use std::{
    ffi::OsString,
    fs::{read_link, File},
    io::{Error as IoError, Write},
    os::unix::io::RawFd,
    path::PathBuf,
};

use nix::{
    errno::Errno,
    fcntl::{fcntl, FcntlArg, FdFlag},
};

use sozu_command_lib::config::Config;
use sozu_lib::metrics::{self, MetricError};

use crate::cli;

#[derive(thiserror::Error, Debug)]
pub enum UtilError {
    #[error("could not get flags (F_GETFD) on file descriptor {0}: {1}")]
    GetFlags(RawFd, Errno),
    #[error("could not convert flags for file descriptor {0}")]
    ConvertFlags(RawFd),
    #[error("could not set flags for file descriptor {0}: {1}")]
    SetFlags(RawFd, Errno),
    #[error("could not create pid file {0}: {1}")]
    CreatePidFile(String, IoError),
    #[error("could not write pid file {0}: {1}")]
    WritePidFile(String, IoError),
    #[error("could not sync pid file {0}: {1}")]
    SyncPidFile(String, IoError),
    #[error("Failed to convert PathBuf {0} to String: {1:?}")]
    OsString(PathBuf, OsString),
    #[error("could not read file {0}: {1}")]
    Read(String, IoError),
    #[error("failed to retrieve current executable path: {0}")]
    CurrentExe(IoError),
    #[error("could not setup metrics: {0}")]
    SetupMetrics(MetricError),
    #[error(
        "Configuration file hasn't been specified. Either use -c with the start command,
    or use the SOZU_CONFIG environment variable when building sozu."
    )]
    GetConfigFilePath,
}

/// FD_CLOEXEC is set by default on every fd in Rust standard lib,
/// so we need to remove the flag on the client, otherwise
/// it won't be accessible
pub fn enable_close_on_exec(fd: RawFd) -> Result<i32, UtilError> {
    let file_descriptor =
        fcntl(fd, FcntlArg::F_GETFD).map_err(|err_no| UtilError::GetFlags(fd, err_no))?;

    let mut new_flags = FdFlag::from_bits(file_descriptor).ok_or(UtilError::ConvertFlags(fd))?;

    new_flags.insert(FdFlag::FD_CLOEXEC);

    fcntl(fd, FcntlArg::F_SETFD(new_flags)).map_err(|err_no| UtilError::SetFlags(fd, err_no))
}

/// FD_CLOEXEC is set by default on every fd in Rust standard lib,
/// so we need to remove the flag on the client, otherwise
/// it won't be accessible
pub fn disable_close_on_exec(fd: RawFd) -> Result<i32, UtilError> {
    let old_flags =
        fcntl(fd, FcntlArg::F_GETFD).map_err(|err_no| UtilError::GetFlags(fd, err_no))?;

    let mut new_flags = FdFlag::from_bits(old_flags).ok_or(UtilError::ConvertFlags(fd))?;

    new_flags.remove(FdFlag::FD_CLOEXEC);

    fcntl(fd, FcntlArg::F_SETFD(new_flags)).map_err(|err_no| UtilError::SetFlags(fd, err_no))
}

pub fn setup_metrics(config: &Config) -> Result<(), UtilError> {
    if let Some(metrics) = config.metrics.as_ref() {
        return metrics::setup(
            &metrics.address,
            "MAIN",
            metrics.tagged_metrics,
            metrics.prefix.clone(),
        )
        .map_err(UtilError::SetupMetrics);
    }
    Ok(())
}

pub fn write_pid_file(config: &Config) -> Result<(), UtilError> {
    let pid_file_path: Option<&str> = config
        .pid_file_path
        .as_ref()
        .map(|pid_file_path| pid_file_path.as_ref());

    if let Some(path) = pid_file_path {
        let mut file = File::create(path)
            .map_err(|io_err| UtilError::CreatePidFile(path.to_owned(), io_err))?;

        let pid = unsafe { libc::getpid() };

        file.write_all(format!("{pid}").as_bytes())
            .map_err(|write_err| UtilError::WritePidFile(path.to_owned(), write_err))?;
        file.sync_all()
            .map_err(|sync_err| UtilError::SyncPidFile(path.to_owned(), sync_err))?;
    }
    Ok(())
}

pub fn get_config_file_path(args: &cli::Args) -> Result<&str, UtilError> {
    match args.config.as_ref() {
        Some(config_file) => Ok(config_file.as_str()),
        None => option_env!("SOZU_CONFIG").ok_or(UtilError::GetConfigFilePath),
    }
}

#[cfg(target_os = "freebsd")]
pub unsafe fn get_executable_path() -> Result<String, UtilError> {
    let mut capacity = PATH_MAX as usize;
    let mut path: Vec<u8> = Vec::with_capacity(capacity);
    path.extend(repeat(0).take(capacity));

    let mib: Vec<i32> = vec![CTL_KERN, KERN_PROC, KERN_PROC_PATHNAME, -1];
    let len = mib.len() * size_of::<i32>();
    let element_size = size_of::<i32>();

    let res = sysctl(
        mib.as_ptr(),
        (len / element_size) as u32,
        path.as_mut_ptr() as *mut c_void,
        &mut capacity,
        std::ptr::null() as *const c_void,
        0,
    );
    if res != 0 {
        panic!("Could not retrieve the path of the executable");
    }

    Ok(String::from_raw_parts(
        path.as_mut_ptr(),
        capacity - 1,
        path.len(),
    ))
}

#[cfg(target_os = "linux")]
pub unsafe fn get_executable_path() -> Result<String, UtilError> {
    let path = read_link("/proc/self/exe")
        .map_err(|io_err| UtilError::Read("/proc/self/exe".to_string(), io_err))?;

    let mut path_str = path
        .clone()
        .into_os_string()
        .into_string()
        .map_err(|string_err| UtilError::OsString(path, string_err))?;

    if path_str.ends_with(" (deleted)") {
        // The kernel appends " (deleted)" to the symlink when the original executable has been replaced
        let len = path_str.len();
        path_str.truncate(len - 10)
    }

    Ok(path_str)
}


#[cfg(target_os = "macos")]
extern "C" {
    pub fn _NSGetExecutablePath(buf: *mut libc::c_char, size: *mut u32) -> i32;
}

#[cfg(target_os = "macos")]
pub unsafe fn get_executable_path() -> Result<String, UtilError> {
    let path = std::env::current_exe().map_err(|io_err| UtilError::CurrentExe(io_err))?;

    Ok(path.to_string_lossy().to_string())
}
