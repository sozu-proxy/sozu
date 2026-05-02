use std::{
    ffi::OsString,
    fs::{File, read_link},
    io::{Error as IoError, Write},
    os::{fd::BorrowedFd, unix::io::RawFd},
    path::PathBuf,
};

#[cfg(target_os = "linux")]
use libc::{cpu_set_t, pid_t};
use nix::{
    errno::Errno,
    fcntl::{FcntlArg, FdFlag, fcntl},
};
use sozu_command_lib::config::Config;
use sozu_lib::metrics::{self, MetricError};

use crate::{cli, command};

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
pub fn enable_close_on_exec(raw_fd: RawFd) -> Result<i32, UtilError> {
    // SAFETY: `BorrowedFd::borrow_raw` requires `raw_fd` to remain open and
    // not be closed by anyone else for the borrow's lifetime. The caller
    // owns `raw_fd` (typically a freshly-created tempfile or unix socket
    // pair end), and we only use `fd` for `fcntl` calls before returning.
    let fd = unsafe { BorrowedFd::borrow_raw(raw_fd) };
    let old_flags =
        fcntl(fd, FcntlArg::F_GETFD).map_err(|err_no| UtilError::GetFlags(raw_fd, err_no))?;

    let mut new_flags = FdFlag::from_bits(old_flags).ok_or(UtilError::ConvertFlags(raw_fd))?;

    new_flags.insert(FdFlag::FD_CLOEXEC);

    fcntl(fd, FcntlArg::F_SETFD(new_flags)).map_err(|err_no| UtilError::SetFlags(raw_fd, err_no))
}

/// FD_CLOEXEC is set by default on every fd in Rust standard lib,
/// so we need to remove the flag on the client, otherwise
/// it won't be accessible
pub fn disable_close_on_exec(raw_fd: RawFd) -> Result<i32, UtilError> {
    // SAFETY: `BorrowedFd::borrow_raw` requires `raw_fd` to remain open and
    // not be closed by anyone else for the borrow's lifetime. The caller
    // owns `raw_fd` (typically a freshly-created tempfile or unix socket
    // pair end), and we only use `fd` for `fcntl` calls before returning.
    let fd = unsafe { BorrowedFd::borrow_raw(raw_fd) };
    let old_flags =
        fcntl(fd, FcntlArg::F_GETFD).map_err(|err_no| UtilError::GetFlags(raw_fd, err_no))?;

    let mut new_flags = FdFlag::from_bits(old_flags).ok_or(UtilError::ConvertFlags(raw_fd))?;

    new_flags.remove(FdFlag::FD_CLOEXEC);

    fcntl(fd, FcntlArg::F_SETFD(new_flags)).map_err(|err_no| UtilError::SetFlags(raw_fd, err_no))
}

pub fn setup_metrics(config: &Config) -> Result<(), UtilError> {
    if let Some(metrics) = config.metrics.as_ref() {
        return metrics::setup(
            &metrics.address,
            "MAIN",
            metrics.tagged_metrics,
            metrics.prefix.clone(),
            metrics.detail,
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

        // SAFETY: `libc::getpid` takes no input pointers, never fails, and
        // returns a value type. No invariant beyond "FFI signature matches libc".
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
/// # Safety
///
/// Calls `sysctl` with raw pointers and reconstructs a `String` from the returned buffer.
pub unsafe fn get_executable_path() -> Result<String, UtilError> {
    use libc::{CTL_KERN, KERN_PROC, KERN_PROC_PATHNAME, PATH_MAX};
    use libc::{c_void, sysctl};

    let mut capacity = PATH_MAX as usize;
    let mut path = vec![0; capacity];

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
/// # Safety
///
/// Reads the current executable path via `/proc/self/exe`, which is only valid for the current process.
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

/// Returns a path string suitable for `execve(2)` that is **race-free against
/// on-disk binary replacement**. Closes [#515].
///
/// The motivating bug: when a master forks a worker (or re-execs itself for
/// hot-upgrade) it calls `Command::new(executable_path).exec()` where
/// `executable_path` is a string like `/usr/bin/sozu`. If the operator has
/// replaced the binary on disk between master startup and the exec call
/// (`cp new-sozu /usr/bin/sozu` followed by `sozuctl upgrade`), `execve(2)`
/// resolves `/usr/bin/sozu` as a normal path and starts the **new** binary,
/// which is incompatible with the running master's protocol expectations.
///
/// On Linux, `/proc/self/exe` is a magic symlink that always resolves to the
/// **original** inode the process was started from, regardless of whether
/// the on-disk file was unlinked or replaced. Opening it with `O_PATH`
/// returns an fd that pins the inode for the lifetime of the fd; passing
/// `/proc/self/fd/<n>` to `execve(2)` causes the kernel to resolve the
/// magic symlink to the original inode, not whatever is currently at the
/// path string.
///
/// We keep the fd open until process exit (the kernel closes it via
/// `O_CLOEXEC` when exec succeeds; if exec fails, the fd persists harmlessly
/// for the rest of the master's lifetime — a bounded, per-exec-attempt leak
/// on a path that is itself a master-in-trouble code path).
///
/// On non-Linux platforms (FreeBSD, macOS) we fall back to the historical
/// path-string approach. The race window is identical to v1.x; the v2.0.0
/// migration target is Linux operators who hit #515 in production.
///
/// [#515]: https://github.com/sozu-proxy/sozu/issues/515
#[cfg(target_os = "linux")]
pub fn get_executable_exec_path() -> Result<String, UtilError> {
    use std::os::fd::IntoRawFd;
    let owned_fd = nix::fcntl::open(
        "/proc/self/exe",
        nix::fcntl::OFlag::O_PATH | nix::fcntl::OFlag::O_CLOEXEC,
        nix::sys::stat::Mode::empty(),
    )
    .map_err(|errno| {
        UtilError::Read(
            "/proc/self/exe".to_string(),
            IoError::from_raw_os_error(errno as i32),
        )
    })?;
    // Convert OwnedFd → RawFd. The fd is intentionally not dropped: it
    // must remain open until exec(2) consumes it. O_CLOEXEC closes it
    // automatically on successful exec; on failed exec the fd persists
    // for the rest of the master's lifetime, which is acceptable on a
    // master-in-trouble code path.
    let raw_fd = owned_fd.into_raw_fd();
    Ok(format!("/proc/self/fd/{raw_fd}"))
}

#[cfg(not(target_os = "linux"))]
pub fn get_executable_exec_path() -> Result<String, UtilError> {
    // FreeBSD / macOS keep the path-string approach. /proc/self/exe is
    // not portable; the existing race window is unchanged on these
    // platforms (out of scope for #515 / v2.0.0).
    // SAFETY: see `get_executable_path` — the path is the running
    // executable's filesystem location.
    unsafe { get_executable_path() }
}

#[cfg(target_os = "macos")]
unsafe extern "C" {
    pub fn _NSGetExecutablePath(buf: *mut libc::c_char, size: *mut u32) -> i32;
}

#[cfg(target_os = "macos")]
/// # Safety
///
/// This is marked unsafe to keep the platform-specific API consistent with other implementations.
pub unsafe fn get_executable_path() -> Result<String, UtilError> {
    let path = std::env::current_exe().map_err(|io_err| UtilError::CurrentExe(io_err))?;

    Ok(path.to_string_lossy().to_string())
}

/// Set workers process affinity, see man sched_setaffinity
/// Bind each worker (including the main) process to a CPU core.
/// Can bind multiple processes to a CPU core if there are more processes
/// than CPU cores. Only works on Linux.
#[cfg(target_os = "linux")]
pub fn set_workers_affinity(workers: &Vec<command::sessions::WorkerSession>) {
    let mut cpu_count = 0;
    let max_cpu = num_cpus::get();

    // +1 for the main process that will also be bound to its CPU core
    if (workers.len() + 1) > max_cpu {
        warn!(
            "There are more workers than available CPU cores, \
          multiple workers will be bound to the same CPU core. \
          This may impact performances"
        );
    }

    // SAFETY: `libc::getpid` takes no input pointers, never fails, and
    // returns a value type. No invariant beyond "FFI signature matches libc".
    let main_pid = unsafe { libc::getpid() };
    set_process_affinity(main_pid, cpu_count);
    cpu_count += 1;

    for worker in workers {
        if cpu_count >= max_cpu {
            cpu_count = 0;
        }

        set_process_affinity(worker.pid, cpu_count);

        cpu_count += 1;
    }
}

/// Set workers process affinity, see man sched_setaffinity
/// Bind each worker (including the main) process to a CPU core.
/// Can bind multiple processes to a CPU core if there are more processes
/// than CPU cores. Only works on Linux.
#[cfg(not(target_os = "linux"))]
pub fn set_workers_affinity(_: &Vec<command::sessions::WorkerSession>) {}

/// Set a specific process to run onto a specific CPU core
#[cfg(target_os = "linux")]
use std::mem;
#[cfg(target_os = "linux")]
pub fn set_process_affinity(pid: pid_t, cpu: usize) {
    // SAFETY: `cpu_set_t` is a C POD; zero-init is a valid bit pattern that
    // produces an empty CPU mask. `CPU_SET` mutates `cpu_set` in place with
    // a valid (compile-time-checked) layout. `sched_setaffinity` reads only
    // the declared `size_cpu_set` bytes; the kernel returns an error code
    // on validation failure (we ignore it here — affinity is best-effort).
    unsafe {
        let mut cpu_set: cpu_set_t = mem::zeroed();
        let size_cpu_set = mem::size_of::<cpu_set_t>();
        libc::CPU_SET(cpu, &mut cpu_set);
        libc::sched_setaffinity(pid, size_cpu_set, &cpu_set);

        debug!("Worker {} bound to CPU core {}", pid, cpu);
    };
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Closes [#515]: `get_executable_exec_path` returns a path string that
    /// resolves to the running binary even when the on-disk file at
    /// `/proc/self/exe`'s symlink target has been replaced.
    ///
    /// This unit test asserts the shape of the returned path and that the
    /// underlying fd is valid (resolvable via the kernel's `/proc/self/fd`).
    /// The race-free property under binary replacement is asserted in the
    /// e2e upgrade-race tests (`e2e/src/tests/upgrade_race_tests.rs` —
    /// follow-up commit).
    ///
    /// [#515]: https://github.com/sozu-proxy/sozu/issues/515
    #[cfg(target_os = "linux")]
    #[test]
    fn get_executable_exec_path_returns_proc_self_fd_on_linux() {
        let path = get_executable_exec_path().expect("open /proc/self/exe O_PATH");
        assert!(
            path.starts_with("/proc/self/fd/"),
            "expected /proc/self/fd/<n>, got {path}"
        );
        // The fd is intentionally leaked for exec, so the path resolves
        // for the rest of the test process. Verify it points at a regular
        // file via `std::fs::metadata` which follows the magic symlink to
        // the original inode.
        let meta =
            std::fs::metadata(&path).unwrap_or_else(|e| panic!("metadata({path}) failed: {e}"));
        assert!(
            meta.is_file(),
            "/proc/self/fd/<n> did not resolve to a regular file"
        );
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn get_executable_exec_path_distinct_calls_yield_distinct_fds() {
        // Each call opens a fresh fd; the path returned is unique per call.
        // This is intentional: callers leak the fd until exec or process
        // exit, so we don't multiplex one fd across multiple call sites.
        let p1 = get_executable_exec_path().expect("first call");
        let p2 = get_executable_exec_path().expect("second call");
        assert_ne!(
            p1, p2,
            "expected distinct fd paths from two opens, got {p1} and {p2}"
        );
    }
}
