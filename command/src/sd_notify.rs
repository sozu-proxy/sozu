//! systemd `sd_notify` integration for the master process.
//!
//! When sozu runs under a systemd unit declared `Type=notify`, the
//! master process tells systemd it is ready (`READY=1`) only AFTER the
//! initial workers have spawned and any saved state has been replayed
//! — without that handshake, `Type=simple` declares the unit ready as
//! soon as the binary forks, which is wrong for sozu's master/worker
//! lifecycle (clients can hit a configured `After=sozu.service` peer
//! before sozu accepts traffic).
//!
//! Implementation is std-only: read `$NOTIFY_SOCKET`, connect a
//! `UnixDatagram`, send the protocol-formatted state line. Closes
//! [#228]. Linux is the only target system where this matters in
//! practice; the helper is a no-op when `$NOTIFY_SOCKET` is unset
//! (e.g. when the operator runs `sozu start` from a shell without
//! systemd in the supervision chain), so non-Linux builds compile and
//! call into the helper without behavioural change.
//!
//! [#228]: https://github.com/sozu-proxy/sozu/issues/228

use std::env;
use std::ffi::{OsStr, OsString};
use std::io;
use std::os::unix::ffi::OsStrExt;
use std::os::unix::net::UnixDatagram;

/// Send a state message to systemd via `$NOTIFY_SOCKET`.
///
/// Supports both filesystem-path sockets (`/run/systemd/notify`) and
/// Linux-abstract sockets (the address starts with `@` per
/// `sd_notify(3)`; common in container images and namespace-gated
/// hosts). Abstract addresses are routed through `libc::sendto` with
/// `AF_UNIX` and a leading NUL byte — `UnixDatagram::connect` would
/// otherwise resolve the path verbatim and fail with `ENOENT`,
/// silently breaking the `Type=notify` readiness handshake.
///
/// Returns `Ok(false)` when `$NOTIFY_SOCKET` is unset (sozu is not
/// running under systemd notify supervision; the caller can ignore
/// this case). Returns `Ok(true)` when the datagram was sent.
/// Returns `Err(...)` for socket / permission failures so the caller
/// can decide whether to log + continue or fail-fast.
///
/// Multiple state lines can be packed into a single message by
/// joining them with `\n` (see [`STATE_READY`] / [`STATE_STOPPING`]).
pub fn notify(state: &str) -> io::Result<bool> {
    let path: OsString = match env::var_os("NOTIFY_SOCKET") {
        Some(p) => p,
        None => return Ok(false),
    };
    let bytes = path.as_bytes();
    if bytes.first() == Some(&b'@') {
        send_abstract(&bytes[1..], state.as_bytes())?;
    } else {
        let sock = UnixDatagram::unbound()?;
        sock.connect::<&OsStr>(path.as_os_str())?;
        sock.send(state.as_bytes())?;
    }
    Ok(true)
}

/// Linux-abstract `AF_UNIX` send. The kernel address layout is a
/// `sun_path` whose first byte is NUL followed by the abstract name
/// (no terminator); `addrlen` covers `sun_family` + 1 + name bytes.
fn send_abstract(name: &[u8], payload: &[u8]) -> io::Result<()> {
    use std::mem::{MaybeUninit, size_of};

    // sun_path is 108 bytes on Linux; minus the leading NUL leaves
    // 107 usable bytes for the abstract name.
    if name.len() > 107 {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "abstract socket name too long",
        ));
    }

    let fd = unsafe { libc::socket(libc::AF_UNIX, libc::SOCK_DGRAM | libc::SOCK_CLOEXEC, 0) };
    if fd < 0 {
        return Err(io::Error::last_os_error());
    }

    let mut addr: MaybeUninit<libc::sockaddr_un> = MaybeUninit::zeroed();
    // SAFETY: the buffer was zero-initialised above; we write the family
    // and the abstract path bytes into the inline `sun_path` array.
    let addrlen = unsafe {
        let p = addr.as_mut_ptr();
        (*p).sun_family = libc::AF_UNIX as libc::sa_family_t;
        let path_ptr = (*p).sun_path.as_mut_ptr() as *mut u8;
        path_ptr.write(0);
        std::ptr::copy_nonoverlapping(name.as_ptr(), path_ptr.add(1), name.len());
        (size_of::<libc::sa_family_t>() + 1 + name.len()) as libc::socklen_t
    };

    let result = unsafe {
        libc::sendto(
            fd,
            payload.as_ptr() as *const libc::c_void,
            payload.len(),
            0,
            addr.as_ptr() as *const libc::sockaddr,
            addrlen,
        )
    };

    let err = if result < 0 {
        Some(io::Error::last_os_error())
    } else {
        None
    };

    // Always close. `close()` errors on a freshly-created datagram
    // socket are not actionable for the caller.
    unsafe {
        libc::close(fd);
    }

    match err {
        Some(e) => Err(e),
        None => Ok(()),
    }
}

/// `READY=1` — master finished spawning workers and loading state.
pub const STATE_READY: &str = "READY=1";

/// `STOPPING=1` — graceful shutdown begins.
pub const STATE_STOPPING: &str = "STOPPING=1";

/// `RELOADING=1` — hot reload begins (the same binary is reloading
/// configuration; for binary upgrade see [`main_pid`]).
pub const STATE_RELOADING: &str = "RELOADING=1";

/// `WATCHDOG=1` — watchdog ping. Currently NOT wired into the master
/// event loop — the bundled `os-build/systemd/sozu.service` and
/// `sozu@.service` units intentionally do NOT declare `WatchdogSec=`,
/// so leaving this constant unused at link time is the documented
/// state. Operators who add `WatchdogSec=` to a custom unit will see
/// the unit get killed for missing pings until the periodic-timer
/// follow-up lands.
#[allow(dead_code)]
pub const STATE_WATCHDOG: &str = "WATCHDOG=1";

/// Send `MAINPID=<pid>` so systemd tracks the new master after a
/// hot-upgrade re-exec hands the supervision off to a forked child.
/// `pid` is unsigned because `MAINPID=` accepts unsigned decimal per
/// `sd_notify(3)`; callers convert from `std::process::id()` directly.
pub fn main_pid(pid: u32) -> io::Result<bool> {
    notify(&format!("MAINPID={pid}"))
}

/// Send `STATUS=<text>` for human-readable status (visible via
/// `systemctl status sozu`).
#[allow(dead_code)]
pub fn status(text: &str) -> io::Result<bool> {
    notify(&format!("STATUS={text}"))
}

#[cfg(test)]
mod tests {
    use super::*;
    use serial_test::serial;
    use std::os::unix::net::UnixDatagram;
    use tempfile::TempDir;

    fn with_notify_socket<R>(value: Option<&OsStr>, body: impl FnOnce() -> R) -> R {
        // SAFETY: cargo runs each `#[test]` on its own thread but the
        // env table is process-shared. `#[serial(notify_socket)]` on
        // the caller serialises every test that touches this env var,
        // so the unsafe mutation cannot race a parallel reader. The
        // previous value is restored on exit so the next test sees a
        // clean slate.
        let prev = env::var_os("NOTIFY_SOCKET");
        unsafe {
            match value {
                Some(v) => env::set_var("NOTIFY_SOCKET", v),
                None => env::remove_var("NOTIFY_SOCKET"),
            }
        }
        let result = body();
        unsafe {
            match prev {
                Some(v) => env::set_var("NOTIFY_SOCKET", v),
                None => env::remove_var("NOTIFY_SOCKET"),
            }
        }
        result
    }

    #[test]
    #[serial(notify_socket)]
    fn notify_no_socket_set_is_noop() {
        let res = with_notify_socket(None, || notify(STATE_READY).expect("noop"));
        assert!(
            !res,
            "notify must report `false` when NOTIFY_SOCKET is unset"
        );
    }

    #[test]
    #[serial(notify_socket)]
    fn notify_writes_payload_to_socket() {
        let dir = TempDir::new().expect("tempdir");
        let socket_path = dir.path().join("notify.sock");
        let listener = UnixDatagram::bind(&socket_path).expect("bind notify socket");

        let sent = with_notify_socket(Some(socket_path.as_os_str()), || {
            notify(STATE_READY).expect("notify must succeed")
        });
        assert!(sent, "notify must report `true` when datagram was sent");

        let mut buf = [0u8; 64];
        let (n, _addr) = listener.recv_from(&mut buf).expect("recv notify datagram");
        assert_eq!(&buf[..n], STATE_READY.as_bytes());
    }

    /// systemd may pass an abstract socket address as `@/path` (the
    /// leading `@` is replaced with NUL inside the kernel address).
    /// `UnixDatagram::connect` would interpret that as a filesystem
    /// path and fail with `ENOENT` — silently breaking `Type=notify`
    /// readiness gating in container images and namespace-gated hosts.
    /// Use `libc::sendto` with `AF_UNIX` + leading-NUL `sun_path`.
    #[test]
    #[serial(notify_socket)]
    fn notify_writes_payload_to_abstract_socket() {
        let abstract_name = format!("sozu-test-notify-{}", std::process::id());
        let listener = bind_abstract(abstract_name.as_bytes()).expect("bind abstract");

        let env_value = format!("@{abstract_name}");
        let sent = with_notify_socket(Some(OsStr::new(&env_value)), || {
            notify(STATE_READY).expect("notify must succeed on abstract socket")
        });
        assert!(sent, "notify must report `true` for abstract socket");

        let mut buf = [0u8; 64];
        let (n, _) = listener
            .recv_from(&mut buf)
            .expect("recv notify datagram on abstract socket");
        assert_eq!(&buf[..n], STATE_READY.as_bytes());
    }

    fn bind_abstract(name: &[u8]) -> io::Result<UnixDatagram> {
        use std::mem::{MaybeUninit, size_of};
        use std::os::fd::FromRawFd;

        let fd = unsafe { libc::socket(libc::AF_UNIX, libc::SOCK_DGRAM | libc::SOCK_CLOEXEC, 0) };
        if fd < 0 {
            return Err(io::Error::last_os_error());
        }
        // SAFETY: zero-init buffer + abstract path layout matches the
        // send path's convention; addrlen covers sun_family + leading
        // NUL byte + name length.
        let (addr, addrlen) = unsafe {
            let mut addr: MaybeUninit<libc::sockaddr_un> = MaybeUninit::zeroed();
            let p = addr.as_mut_ptr();
            (*p).sun_family = libc::AF_UNIX as libc::sa_family_t;
            let path_ptr = (*p).sun_path.as_mut_ptr() as *mut u8;
            path_ptr.write(0);
            std::ptr::copy_nonoverlapping(name.as_ptr(), path_ptr.add(1), name.len());
            (
                addr,
                (size_of::<libc::sa_family_t>() + 1 + name.len()) as libc::socklen_t,
            )
        };
        let res = unsafe { libc::bind(fd, addr.as_ptr() as *const libc::sockaddr, addrlen) };
        if res < 0 {
            let err = io::Error::last_os_error();
            unsafe { libc::close(fd) };
            return Err(err);
        }
        // SAFETY: fd was just created by `socket(2)` and bound; we hand
        // ownership to UnixDatagram which closes it on drop.
        Ok(unsafe { UnixDatagram::from_raw_fd(fd) })
    }
}
