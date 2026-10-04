use std::{
    ffi::OsString,
    fs::{File, OpenOptions, read_link},
    io::{Error as IoError, Write},
    os::{fd::BorrowedFd, unix::io::RawFd},
    path::{Path, PathBuf},
};

#[cfg(target_os = "linux")]
use libc::{cpu_set_t, pid_t};
use nix::{
    errno::Errno,
    fcntl::{FcntlArg, FdFlag, fcntl},
    unistd::{AccessFlags, access},
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

/// Check that the configured pid file can be published later by
/// [`publish_pid_file`], without changing anything on disk.
///
/// A replacement main calls this before PREPARED, while a failure still rolls
/// back to the old main: an unwritable path (a directory, a missing parent, a
/// read-only file system, a denied permission) fails here instead of after
/// COMMIT. An existing file is opened for writing but never truncated, so the
/// old main's pid stays published if the upgrade rolls back. A missing file
/// is not created: only its parent directory is checked for write and search
/// access, and [`publish_pid_file`] creates it after COMMIT. Creating it here
/// would leave an empty pid file behind every rolled-back upgrade, including
/// one where the old main SIGKILLs this process before it could clean up.
pub fn open_pid_file(config: &Config) -> Result<Option<(String, Option<File>)>, UtilError> {
    let Some(path) = config.pid_file_path.as_deref() else {
        return Ok(None);
    };
    match OpenOptions::new().write(true).truncate(false).open(path) {
        Ok(file) => Ok(Some((path.to_owned(), Some(file)))),
        Err(io_err) if io_err.kind() == std::io::ErrorKind::NotFound => {
            let parent = match Path::new(path).parent() {
                Some(parent) if !parent.as_os_str().is_empty() => parent,
                _ => Path::new("."),
            };
            access(parent, AccessFlags::W_OK | AccessFlags::X_OK)
                .map_err(|errno| UtilError::CreatePidFile(path.to_owned(), IoError::from(errno)))?;
            Ok(Some((path.to_owned(), None)))
        }
        Err(io_err) => Err(UtilError::CreatePidFile(path.to_owned(), io_err)),
    }
}

/// Replace the content of the pid file checked by [`open_pid_file`] with this
/// process's pid, creating it when it did not exist then.
pub fn publish_pid_file(path: &str, file: Option<File>) -> Result<(), UtilError> {
    let mut file = match file {
        Some(file) => file,
        None => OpenOptions::new()
            .write(true)
            .create(true)
            .truncate(false)
            .open(path)
            .map_err(|io_err| UtilError::CreatePidFile(path.to_owned(), io_err))?,
    };
    // SAFETY: `libc::getpid` takes no input pointers, never fails, and
    // returns a value type. No invariant beyond "FFI signature matches libc".
    let pid = unsafe { libc::getpid() };

    file.set_len(0)
        .map_err(|write_err| UtilError::WritePidFile(path.to_owned(), write_err))?;
    file.write_all(format!("{pid}").as_bytes())
        .map_err(|write_err| UtilError::WritePidFile(path.to_owned(), write_err))?;
    file.sync_all()
        .map_err(|sync_err| UtilError::SyncPidFile(path.to_owned(), sync_err))
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
/// on-disk binary replacement**. Used by the **worker auto-restart** path
/// only. Closes [#515].
///
/// **Scope** — read this before adding a new call site.
///
/// Sōzu has two `Command::new(...).exec()` sites that re-launch the binary:
///
/// | Site | Intent | Should use this helper? |
/// |------|--------|-------------------------|
/// | [`bin/src/worker.rs`] worker auto-restart | spawn a worker matching the **running master's** version | **YES** — race-free fd-based exec |
/// | [`bin/src/upgrade.rs`] master hot-upgrade | switch to the **new** on-disk binary the operator just installed | **NO** — path-based exec is the operator's intent |
///
/// The motivating bug: a master at version A spawned a worker via
/// `Command::new(executable_path).exec()` where `executable_path` was a
/// string like `/usr/bin/sozu`. If a package upgrade had replaced the
/// on-disk binary with version B between master startup and a worker
/// auto-restart, `execve(2)` resolved `/usr/bin/sozu` as a normal path and
/// started version B, incompatible with the master at version A.
///
/// The fix on Linux: `/proc/self/exe` is a magic symlink that always
/// resolves to the **original** inode the process was started from,
/// regardless of whether the on-disk file was unlinked or replaced.
/// Opening it with `O_PATH` returns an fd that pins the inode for the
/// lifetime of the fd; passing `/proc/self/fd/<n>` to `execve(2)` causes
/// the kernel to resolve the magic symlink to the original inode, not
/// whatever is currently at the path string. Workers spawned via this
/// helper therefore always match the running master's version.
///
/// The master hot-upgrade path deliberately skips this helper because the
/// operator's whole point is to switch to a different version. There the
/// path-based `Command::new(executable_path)` is correct.
///
/// On successful exec the kernel closes the fd via `O_CLOEXEC`. On failed
/// exec the forked child returns `Err(WorkerError::SpawnChild(...))` and
/// the worker spawn aborts, so the fd cannot accumulate across spawn
/// attempts — the helper is called inside the child immediately before
/// `execve(2)`, never in the long-running master.
///
/// On non-Linux platforms (FreeBSD, macOS) we fall back to the historical
/// path-string approach. The race window is identical to before; the
/// migration target is Linux operators who hit #515 in production.
///
/// [#515]: https://github.com/sozu-proxy/sozu/issues/515
/// [`bin/src/worker.rs`]: ../worker/index.html
/// [`bin/src/upgrade.rs`]: ../upgrade/index.html
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
    // automatically on successful exec; on failed exec the forked child
    // returns Err and aborts the spawn, so no leak persists on the
    // long-running master.
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

/// Kernel clocksources that can set a `vdso_clock_mode` other than
/// `VDSO_CLOCKMODE_NONE`, so that the vDSO reads them in user space: `tsc`
/// (x86), `kvm-clock` and `xen` (KVM and Xen guests, when the host reports a
/// stable TSC), `hyperv_clocksource_tsc_page` (Hyper-V guests),
/// `arch_sys_counter` (arm64) and `riscv_clocksource` (RISC-V). Any other
/// source, `hpet`, `acpi_pm` or `jiffies` among them, turns every
/// `clock_gettime` behind `Instant::now()` into a real syscall. The name is a
/// heuristic: a listed source can still lose its vDSO mode at runtime.
const FAST_CLOCKSOURCES: &[&str] = &[
    "tsc",
    "kvm-clock",
    "xen",
    "hyperv_clocksource_tsc_page",
    "arch_sys_counter",
    "riscv_clocksource",
];

const CURRENT_CLOCKSOURCE: &str =
    "/sys/devices/system/clocksource/clocksource0/current_clocksource";

/// Warning to log when `clocksource`, the content of the sysfs
/// `current_clocksource` file, makes `clock_gettime` a syscall.
/// An empty value is not a verdict and yields no warning.
pub fn slow_clocksource_warning(clocksource: &str) -> Option<String> {
    let clocksource = clocksource.trim();
    if clocksource.is_empty() || FAST_CLOCKSOURCES.contains(&clocksource) {
        return None;
    }
    Some(format!(
        "the kernel clocksource is '{clocksource}', which the vDSO cannot read: every \
         Instant::now() Sōzu takes for timeouts and metrics becomes a clock_gettime syscall, \
         which can dominate CPU time under load (see issue #500). Compare \
         {CURRENT_CLOCKSOURCE} with its sibling available_clocksource and, if one of {} is \
         listed, switch to it (echo <source> > {CURRENT_CLOCKSOURCE}, or the clocksource= \
         kernel parameter)",
        FAST_CLOCKSOURCES.join(", ")
    ))
}

/// Log a warning once, at main-process startup, when the kernel clocksource
/// is slow to read. An unreadable sysfs file is skipped silently.
#[cfg(target_os = "linux")]
pub fn warn_on_slow_clocksource() {
    match std::fs::read_to_string(CURRENT_CLOCKSOURCE) {
        Ok(clocksource) => match slow_clocksource_warning(&clocksource) {
            Some(warning) => warn!("{}", warning),
            None => debug!("kernel clocksource: {}", clocksource.trim()),
        },
        Err(e) => debug!("could not read {}: {}", CURRENT_CLOCKSOURCE, e),
    }
}

#[cfg(not(target_os = "linux"))]
pub fn warn_on_slow_clocksource() {}

#[cfg(test)]
mod tests {
    use super::*;

    /// Closes [#515]: `get_executable_exec_path` returns a path string that
    /// resolves to the running binary even when the on-disk file at
    /// `/proc/self/exe`'s symlink target has been replaced.
    ///
    /// This unit test asserts the shape of the returned path and that the
    /// underlying fd is valid (resolvable via the kernel's `/proc/self/fd`).
    /// The race-free property under binary replacement follows from the
    /// kernel-tested magic-symlink semantics of `/proc/self/exe`; this
    /// unit test asserts the helper's shape only.
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

    #[test]
    fn slow_clocksource_warning_accepts_vdso_capable_sources() {
        for source in FAST_CLOCKSOURCES {
            assert_eq!(slow_clocksource_warning(source), None, "{source}");
        }
        for source in [
            "tsc",
            "kvm-clock",
            "arch_sys_counter",
            "hyperv_clocksource_tsc_page",
        ] {
            assert!(FAST_CLOCKSOURCES.contains(&source), "{source}");
            assert_eq!(slow_clocksource_warning(source), None, "{source}");
        }
    }

    #[test]
    fn slow_clocksource_warning_flags_syscall_only_sources() {
        for source in ["hpet", "acpi_pm", "jiffies"] {
            let warning = slow_clocksource_warning(source).expect(source);
            assert!(warning.contains(&format!("'{source}'")), "{warning}");
            assert!(warning.contains("current_clocksource"), "{warning}");
        }
    }

    #[test]
    fn slow_clocksource_warning_trims_the_sysfs_newline() {
        assert_eq!(slow_clocksource_warning("tsc\n"), None);
        assert_eq!(slow_clocksource_warning("  kvm-clock \n"), None);
        let warning = slow_clocksource_warning("hpet\n").expect("hpet is slow");
        assert!(warning.contains("'hpet'"), "{warning}");
        assert!(!warning.contains("hpet\n"), "{warning}");
        assert_eq!(slow_clocksource_warning(" \n"), None);
    }

    fn pid_file_config(path: &std::path::Path) -> Config {
        Config {
            pid_file_path: Some(path.to_str().expect("utf-8 temp path").to_owned()),
            ..Default::default()
        }
    }

    /// A rolled-back main upgrade must not leave an empty pid file behind:
    /// checking a missing pid file creates nothing, and only the post-COMMIT
    /// publish writes it.
    #[test]
    fn open_pid_file_defers_creating_a_missing_file_until_publish() {
        let temp = tempfile::tempdir().expect("tempdir");
        let path = temp.path().join("sozu.pid");

        let (published_path, file) = open_pid_file(&pid_file_config(&path))
            .expect("a writable parent directory is accepted")
            .expect("a configured pid file is returned");
        assert!(file.is_none());
        assert!(
            !path.exists(),
            "the pre-COMMIT check must not create the file"
        );

        publish_pid_file(&published_path, file).expect("publish creates the file");
        assert_eq!(
            std::fs::read_to_string(&path).expect("read pid file"),
            std::process::id().to_string()
        );
    }

    #[test]
    fn open_pid_file_keeps_an_existing_file_until_publish() {
        let temp = tempfile::tempdir().expect("tempdir");
        let path = temp.path().join("sozu.pid");
        std::fs::write(&path, "123456789").expect("seed pid file");

        let (published_path, file) = open_pid_file(&pid_file_config(&path))
            .expect("an existing writable file is accepted")
            .expect("a configured pid file is returned");
        assert!(file.is_some());
        assert_eq!(std::fs::read_to_string(&path).unwrap(), "123456789");

        publish_pid_file(&published_path, file).expect("publish rewrites the file");
        assert_eq!(
            std::fs::read_to_string(&path).unwrap(),
            std::process::id().to_string()
        );
    }

    #[test]
    fn open_pid_file_rejects_a_missing_parent_directory() {
        let temp = tempfile::tempdir().expect("tempdir");
        let path = temp.path().join("missing").join("sozu.pid");

        assert!(matches!(
            open_pid_file(&pid_file_config(&path)),
            Err(UtilError::CreatePidFile(_, _))
        ));
        assert!(!path.exists());
    }
}
